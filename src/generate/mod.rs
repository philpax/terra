use crate::srgb::SRGB_TO_LINEAR;
use crate::terrain::dem::DemSource;
use crate::terrain::quadtree::VNode;
use crate::terrain::raster::RasterCache;
use crate::terrain::tile_cache::{LayerParams, LayerType, TextureFormat};
use crate::{
    cache::{AssetLoadContext, AssetLoadContextBuf, WebAsset},
    gpu_state::GpuState,
};
use crate::{coordinates, Terrain};
use crate::{
    mapfile::{MapFile, TextureDescriptor},
    terrain::raster::GlobalRaster,
};
use anyhow::Error;
use bytemuck::Pod;
use cgmath::Vector2;
use futures::StreamExt;
use image::{png::PngDecoder, ColorType, ImageDecoder};
use itertools::Itertools;
use maplit::hashmap;
use rayon::prelude::*;
use std::{f64::consts::PI, fs::File, path::PathBuf};
use std::{
    io::{Read, Write},
    path::Path,
    sync::{Arc, Mutex},
};
use vec_map::VecMap;

mod gpu;
pub mod heightmap;

pub(crate) use gpu::*;

/// The radius of the earth in meters.
pub(crate) const EARTH_RADIUS: f64 = 6371000.0;
pub(crate) const EARTH_CIRCUMFERENCE: f64 = 2.0 * PI * EARTH_RADIUS;

pub const BLUE_MARBLE_URLS: [&str; 8] = [
    "https://eoimages.gsfc.nasa.gov/images/imagerecords/76000/76487/world.200406.3x21600x21600.A1.png",
    "https://eoimages.gsfc.nasa.gov/images/imagerecords/76000/76487/world.200406.3x21600x21600.A2.png",
    "https://eoimages.gsfc.nasa.gov/images/imagerecords/76000/76487/world.200406.3x21600x21600.B1.png",
    "https://eoimages.gsfc.nasa.gov/images/imagerecords/76000/76487/world.200406.3x21600x21600.B2.png",
    "https://eoimages.gsfc.nasa.gov/images/imagerecords/76000/76487/world.200406.3x21600x21600.C1.png",
    "https://eoimages.gsfc.nasa.gov/images/imagerecords/76000/76487/world.200406.3x21600x21600.C2.png",
    "https://eoimages.gsfc.nasa.gov/images/imagerecords/76000/76487/world.200406.3x21600x21600.D1.png",
    "https://eoimages.gsfc.nasa.gov/images/imagerecords/76000/76487/world.200406.3x21600x21600.D2.png",
];

pub(crate) trait GenerateTile {
    /// Layers generated by this object. Zero means generate cannot operate for nodes of this level.
    fn outputs(&self, level: u8) -> u32;
    /// Layers required to be present at `level` when generating a tile at `level`.
    fn peer_inputs(&self, level: u8) -> u32;
    /// Layers required to be present at `level-1` when generating a tile at `level`.
    fn parent_inputs(&self, level: u8) -> u32;
    /// Returns whether previously generated tiles from this generator are still valid.
    fn needs_refresh(&mut self) -> bool;
    /// Run the generator for `node`.
    fn generate(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        state: &GpuState,
        layers: &VecMap<LayerParams>,
        node: VNode,
        slot: usize,
        parent_slot: Option<usize>,
        output_mask: u32,
    );
}

struct ShaderGen<T, F: 'static + Fn(VNode, usize, Option<usize>, u32) -> T> {
    shader: ComputeShader<T>,
    dimensions: u32,
    peer_inputs: u32,
    parent_inputs: u32,
    outputs: u32,
    /// Used instead of outputs for root nodes
    root_outputs: u32,
    /// Used instead of peer_inputs for root nodes
    root_peer_inputs: u32,
    blit_from_bc5_staging: Option<LayerType>,
    f: F,
}
impl<T: Pod, F: 'static + Fn(VNode, usize, Option<usize>, u32) -> T> GenerateTile
    for ShaderGen<T, F>
{
    fn outputs(&self, level: u8) -> u32 {
        if level > 0 {
            self.outputs
        } else {
            self.root_outputs
        }
    }
    fn peer_inputs(&self, level: u8) -> u32 {
        if level > 0 {
            self.peer_inputs
        } else {
            self.root_peer_inputs
        }
    }
    fn parent_inputs(&self, level: u8) -> u32 {
        if level > 0 {
            self.parent_inputs
        } else {
            0
        }
    }
    fn needs_refresh(&mut self) -> bool {
        self.shader.refresh()
    }
    fn generate(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        state: &GpuState,
        layers: &VecMap<LayerParams>,
        node: VNode,
        slot: usize,
        parent_slot: Option<usize>,
        output_mask: u32,
    ) {
        let uniforms = (self.f)(node, slot, parent_slot, output_mask);
        self.shader.run(device, encoder, state, (self.dimensions, self.dimensions, 1), &uniforms);
        if let Some(layer) = self.blit_from_bc5_staging {
            let resolution = layers[layer].texture_resolution;
            assert!(resolution <= 1024);
            assert!(resolution % 4 == 0);
            let buffer = device.create_buffer(&wgpu::BufferDescriptor {
                size: 1024 * 1024 * 4, // TODO
                usage: wgpu::BufferUsage::COPY_SRC | wgpu::BufferUsage::COPY_DST,
                mapped_at_creation: false,
                label: None,
            });
            encoder.copy_texture_to_buffer(
                wgpu::TextureCopyView {
                    texture: &state.bc5_staging,
                    mip_level: 0,
                    origin: wgpu::Origin3d::default(),
                },
                wgpu::BufferCopyView {
                    buffer: &buffer,
                    layout: wgpu::TextureDataLayout {
                        bytes_per_row: 4096,
                        rows_per_image: resolution as u32 / 4,
                        offset: 0,
                    },
                },
                wgpu::Extent3d {
                    width: resolution as u32 / 4,
                    height: resolution as u32 / 4,
                    depth: 1,
                },
            );
            encoder.copy_buffer_to_texture(
                wgpu::BufferCopyView {
                    buffer: &buffer,
                    layout: wgpu::TextureDataLayout {
                        bytes_per_row: 4096,
                        rows_per_image: resolution,
                        offset: 0,
                    },
                },
                wgpu::TextureCopyView {
                    texture: &state.tile_cache[LayerType::Normals],
                    mip_level: 0,
                    origin: wgpu::Origin3d { x: 0, y: 0, z: slot as u32 },
                },
                wgpu::Extent3d { width: resolution, height: resolution, depth: 1 },
            );
        }
    }
}

struct ShaderGenBuilder {
    dimensions: u32,
    shader: rshader::ShaderSource,
    peer_inputs: u32,
    parent_inputs: u32,
    outputs: u32,
    root_outputs: Option<u32>,
    root_peer_inputs: Option<u32>,
    blit_from_bc5_staging: Option<LayerType>,
}
impl ShaderGenBuilder {
    fn new(shader: rshader::ShaderSource) -> Self {
        Self {
            dimensions: 0,
            outputs: 0,
            shader,
            peer_inputs: 0,
            parent_inputs: 0,
            root_outputs: None,
            root_peer_inputs: None,
            blit_from_bc5_staging: None,
        }
    }
    fn dimensions(mut self, dimensions: u32) -> Self {
        self.dimensions = dimensions;
        self
    }
    fn outputs(mut self, outputs: u32) -> Self {
        self.outputs = outputs;
        self
    }
    fn root_outputs(mut self, root_outputs: u32) -> Self {
        self.root_outputs = Some(root_outputs);
        self
    }
    fn root_peer_inputs(mut self, root_peer_inputs: u32) -> Self {
        self.root_peer_inputs = Some(root_peer_inputs);
        self
    }
    fn peer_inputs(mut self, peer_inputs: u32) -> Self {
        self.peer_inputs = peer_inputs;
        self
    }
    fn parent_inputs(mut self, parent_inputs: u32) -> Self {
        self.parent_inputs = parent_inputs;
        self
    }
    fn blit_from_bc5_staging(mut self, layer: LayerType) -> Self {
        self.blit_from_bc5_staging = Some(layer);
        self
    }
    fn build<T: Pod, F: 'static + Fn(VNode, usize, Option<usize>, u32) -> T>(
        self,
        device: &wgpu::Device,
        f: F,
    ) -> Box<dyn GenerateTile> {
        Box::new(ShaderGen {
            shader: ComputeShader::new(
                device,
                rshader::ShaderSet::compute_only(self.shader).unwrap(),
            ),
            outputs: self.outputs,
            peer_inputs: self.peer_inputs,
            parent_inputs: self.parent_inputs,
            dimensions: self.dimensions,
            root_outputs: self.root_outputs.unwrap_or(if self.parent_inputs == 0 {
                self.outputs
            } else {
                0
            }),
            root_peer_inputs: self.root_peer_inputs.unwrap_or(self.peer_inputs),
            blit_from_bc5_staging: self.blit_from_bc5_staging,
            f,
        })
    }
}

pub(crate) fn generators(
    layers: &VecMap<LayerParams>,
    device: &wgpu::Device,
) -> Vec<Box<dyn GenerateTile>> {
    let heightmaps_resolution = layers[LayerType::Heightmaps].texture_resolution;
    let heightmaps_border = layers[LayerType::Heightmaps].texture_border_size;
    let displacements_resolution = layers[LayerType::Displacements].texture_resolution;
    let normals_resolution = layers[LayerType::Normals].texture_resolution;
    let normals_border = layers[LayerType::Normals].texture_border_size;

    vec![
        ShaderGenBuilder::new(rshader::shader_source!(
            "../shaders",
            "version",
            "hash",
            "gen-heightmaps.comp"
        ))
        .outputs(LayerType::Heightmaps.bit_mask())
        .dimensions((heightmaps_resolution + 7) / 8)
        .parent_inputs(LayerType::Heightmaps.bit_mask())
        .build(
            device,
            move |node: VNode,
                  slot: usize,
                  parent_slot: Option<usize>,
                  _|
                  -> GenHeightmapsUniforms {
                let (_parent, parent_index) = node.parent().expect("root node missing");
                let parent_offset = crate::terrain::quadtree::node::OFFSETS[parent_index as usize];
                let origin = [
                    heightmaps_border as i32 / 2,
                    heightmaps_resolution as i32 / 2 - heightmaps_border as i32 / 2,
                ];
                let spacing = node.aprox_side_length()
                    / (heightmaps_resolution - heightmaps_border * 2 - 1) as f32;
                let resolution = heightmaps_resolution - heightmaps_border * 2 - 1;
                let level_resolution = resolution << node.level();
                GenHeightmapsUniforms {
                    position: [
                        (node.x() * resolution) as i32
                            - level_resolution as i32 / 2
                            - heightmaps_border as i32,
                        (node.y() * resolution) as i32
                            - level_resolution as i32 / 2
                            - heightmaps_border as i32,
                    ],
                    origin: [origin[parent_offset.x as usize], origin[parent_offset.y as usize]],
                    spacing,
                    in_slot: parent_slot.unwrap() as i32,
                    out_slot: slot as i32,
                    level_resolution: level_resolution as i32,
                    face: node.face() as u32,
                }
            },
        ),
        ShaderGenBuilder::new(if cfg!(feature = "soft-float64") {
            rshader::shader_source!(
                "../shaders",
                "version",
                "softdouble.glsl",
                "gen-displacements.comp"
            )
        } else {
            rshader::shader_source!("../shaders", "version", "gen-displacements.comp")
        })
        .outputs(LayerType::Displacements.bit_mask())
        .root_outputs(LayerType::Displacements.bit_mask())
        .dimensions((displacements_resolution + 7) / 8)
        .parent_inputs(LayerType::Heightmaps.bit_mask())
        .root_peer_inputs(LayerType::Heightmaps.bit_mask())
        .build(
            device,
            move |node: VNode,
                  slot: usize,
                  parent_slot: Option<usize>,
                  _|
                  -> GenDisplacementsUniforms {
                let base_stride = (heightmaps_resolution - heightmaps_border * 2 - 1)
                    / (displacements_resolution - 1);
                let (offset, stride) = match parent_slot {
                    Some(_) => (Vector2::new(node.x() & 1, node.y() & 1), base_stride / 2),
                    None => (Vector2::new(0, 0), base_stride),
                };
                let world_center = node.center_wspace();
                let resolution = displacements_resolution - 1;
                let level_resolution = resolution << node.level();
                GenDisplacementsUniforms {
                    node_center: world_center.into(),
                    origin: [
                        (heightmaps_border
                            + (heightmaps_resolution - heightmaps_border * 2 - 1) * offset.x / 2)
                            as i32,
                        (heightmaps_border
                            + (heightmaps_resolution - heightmaps_border * 2 - 1) * offset.y / 2)
                            as i32,
                    ],
                    stride: stride as i32,
                    displacements_slot: slot as i32,
                    heightmaps_slot: parent_slot.unwrap_or(slot) as i32,
                    position: [
                        (node.x() * resolution) as i32 - level_resolution as i32 / 2,
                        (node.y() * resolution) as i32 - level_resolution as i32 / 2,
                    ],
                    face: node.face() as i32,
                    level_resolution,
                    padding0: 0.0,
                }
            },
        ),
        ShaderGenBuilder::new(rshader::shader_source!(
            "../shaders",
            "version",
            "hash",
            "gen-normals.comp"
        ))
        .outputs(LayerType::Normals.bit_mask() | LayerType::Albedo.bit_mask())
        .root_outputs(LayerType::Normals.bit_mask())
        .dimensions((normals_resolution + 3) / 4)
        .parent_inputs(LayerType::Albedo.bit_mask())
        .peer_inputs(LayerType::Heightmaps.bit_mask())
        .blit_from_bc5_staging(LayerType::Normals)
        .build(
            device,
            move |node: VNode,
                  slot: usize,
                  parent_slot: Option<usize>,
                  output_mask: u32|
                  -> GenNormalsUniforms {
                let spacing =
                    node.aprox_side_length() / (normals_resolution - normals_border * 2) as f32;

                let albedo_slot =
                    if output_mask & LayerType::Albedo.bit_mask() != 0 { slot as i32 } else { -1 };

                let parent_index = node.parent().map(|(_, idx)| idx).unwrap_or(0);

                GenNormalsUniforms {
                    heightmaps_origin: [
                        (heightmaps_border - normals_border) as i32,
                        (heightmaps_border - normals_border) as i32,
                    ],
                    spacing,
                    heightmaps_slot: slot as i32,
                    normals_slot: slot as i32,
                    albedo_slot,
                    parent_slot: parent_slot.map(|s| s as i32).unwrap_or(-1),
                    parent_origin: [
                        if parent_index % 2 == 0 {
                            normals_border / 2
                        } else {
                            (normals_resolution - normals_border) / 2
                        },
                        if parent_index / 2 == 0 {
                            normals_border / 2
                        } else {
                            (normals_resolution - normals_border) / 2
                        },
                    ],
                    padding: 0,
                }
            },
        ),
    ]
}

pub(crate) struct MapFileBuilder(MapFile);
impl MapFileBuilder {
    pub(crate) fn new() -> Self {
        let layers: VecMap<LayerParams> = hashmap![
            LayerType::Heightmaps.index() => LayerParams {
                    layer_type: LayerType::Heightmaps,
                    texture_resolution: 521,
                    texture_border_size: 4,
                    texture_format: TextureFormat::R32F,
                    tiles_generated_per_frame: 16,
                    // peer_dependency_mask: 0,
                    // parent_dependency_mask: LayerType::Heightmaps.bit_mask(),
                },
            LayerType::Displacements.index() => LayerParams {
                    layer_type: LayerType::Displacements,
                    texture_resolution: 65,
                    texture_border_size: 0,
                    texture_format: TextureFormat::RGBA32F,
                    tiles_generated_per_frame: 128,
                    // peer_dependency_mask: 0,
                    // parent_dependency_mask: LayerType::Heightmaps.bit_mask(),
                },
            LayerType::Albedo.index() => LayerParams {
                    layer_type: LayerType::Albedo,
                    texture_resolution: 516,
                    texture_border_size: 2,
                    texture_format: TextureFormat::RGBA8,
                    tiles_generated_per_frame: 16,
                    // peer_dependency_mask: 0,
                    // parent_dependency_mask: LayerType::Albedo.bit_mask(),
                },
            LayerType::Roughness.index() => LayerParams {
                    layer_type: LayerType::Roughness,
                    texture_resolution: 516,
                    texture_border_size: 2,
                    texture_format: TextureFormat::BC4,
                    tiles_generated_per_frame: 16,
                    // peer_dependency_mask: 0,
                    // parent_dependency_mask: LayerType::Roughness.bit_mask(),
                },
            LayerType::Normals.index() => LayerParams {
                    layer_type: LayerType::Normals,
                    texture_resolution: 516,
                    texture_border_size: 2,
                    texture_format: TextureFormat::BC5,
                    tiles_generated_per_frame: 16,
                    // peer_dependency_mask: LayerType::Heightmaps.bit_mask(),
                    // parent_dependency_mask: LayerType::Albedo.bit_mask(),
                },
        ]
        .into_iter()
        .collect();

        let mapfile = MapFile::new(layers);
        VNode::breadth_first(|n| {
            mapfile.reload_tile_state(LayerType::Heightmaps, n, true).unwrap();
            n.level() < VNode::LEVEL_CELL_153M
        });
        VNode::breadth_first(|n| {
            mapfile.reload_tile_state(LayerType::Albedo, n, true).unwrap();
            n.level() < VNode::LEVEL_CELL_625M
        });
        VNode::breadth_first(|n| {
            mapfile.reload_tile_state(LayerType::Roughness, n, true).unwrap();
            false
        });

        Self(mapfile)
    }

    /// Actually construct the `QuadTree`.
    ///
    /// This function will (the first time it is called) download many gigabytes of raw data,
    /// primarily datasets relating to real world land cover and elevation. These files will be
    /// stored in ~/.cache/terra, so that they don't have to be fetched multiple times. This means that
    /// this function can largely resume from where it left off if interrupted.
    ///
    /// Even once all needed files have been downloaded, the generation process takes a large amount
    /// of CPU resources. You can expect it to run at full load continiously for several full
    /// minutes, even in release builds (you *really* don't want to wait for generation in debug
    /// mode...).
    pub(crate) async fn build(mut self) -> Result<MapFile, Error> {
        let mut context = AssetLoadContextBuf::new();
        let mut context = context.context("Building Terrain...", 1);
        // generate_heightmaps(&mut mapfile, &mut context).await?;
        // generate_albedo(&mut mapfile, &mut context)?;
        // generate_roughness(&mut mapfile, &mut context)?;
        generate_noise(&mut self.0, &mut context)?;
        generate_sky(&mut self.0, &mut context)?;

        Ok(self.0)
    }
}

impl Terrain {
    /// Generate heightmap tiles.
    ///
    /// `etopo1_file` is the location of [ETOPO1_Ice_c_geotiff.zip](https://www.ngdc.noaa.gov/mgg/global/relief/ETOPO1/data/ice_surface/cell_registered/georeferenced_tiff/ETOPO1_Ice_c_geotiff.zip).
    pub async fn generate_heightmaps<'a, F: FnMut(&str, usize, usize) + Send>(
        &mut self,
        etopo1_file: impl AsRef<Path>,
        srtm3_directory: PathBuf,
        mut progress_callback: F,
    ) -> Result<(), Error> {
        let (missing, total_tiles) = self.mapfile.get_missing_base(LayerType::Heightmaps)?;
        if missing.is_empty() {
            return Ok(());
        }

        let mut gen = heightmap::HeightmapGen {
            tile_cache: heightmap::HeightmapCache::new(
                self.mapfile.layers()[LayerType::Heightmaps].clone(),
                32,
            ),
            dems: RasterCache::new(Arc::new(DemSource::Srtm90m(srtm3_directory)), 256),
            global_dem: Arc::new(crate::terrain::dem::parse_etopo1(
                etopo1_file,
                &mut progress_callback,
            )?),
        };

        let total_missing = missing.len();
        let mut missing_by_level = VecMap::new();
        for m in missing {
            missing_by_level.entry(m.level().into()).or_insert(Vec::new()).push(m);
        }

        let mut tiles_processed = 0;
        for missing in missing_by_level.values() {
            let mut missing = missing.into_iter().peekable();
            let mut pending = futures::stream::FuturesUnordered::new();

            loop {
                if pending.len() < 16 && missing.peek().is_some() {
                    pending.push(
                        gen.generate_heightmaps(
                            Arc::clone(&self.mapfile),
                            *missing.next().unwrap(),
                        )
                        .await?,
                    );
                } else {
                    match pending.next().await {
                        Some(result) => {
                            result?;
                            tiles_processed += 1;
                            progress_callback(
                                "Generating heightmaps...",
                                tiles_processed + (total_tiles - total_missing),
                                total_tiles,
                            );
                        }
                        None => break,
                    }
                }
            }
        }

        Ok(())
    }

    /// Generate albedo tiles.
    ///
    /// `blue_marble_directory` must contain the 8 files from NASA's Blue Marble: Next Generation
    /// indicated in [`BLUE_MARBLE_URLS`](constant.BLUE_MARBLE_URLS.html).
    pub async fn generate_albedos<F: FnMut(&str, usize, usize) + Send>(
        &mut self,
        blue_marble_directory: impl AsRef<Path>,
        mut progress_callback: F,
    ) -> Result<(), Error> {
        let (missing, total_tiles) = self.mapfile.get_missing_base(LayerType::Albedo)?;
        if missing.is_empty() {
            return Ok(());
        }

        let layer = self.mapfile.layers()[LayerType::Albedo].clone();
        assert!(layer.texture_border_size >= 2);

        let bm_dimensions = 21600;
        let mut values = vec![0u8; bm_dimensions * bm_dimensions * 8 * 3];

        let (north, south) = values.split_at_mut(bm_dimensions * bm_dimensions * 12);
        let mut slices: Vec<&mut [u8]> = north
            .chunks_exact_mut(bm_dimensions * 3)
            .interleave(south.chunks_exact_mut(bm_dimensions * 3))
            .collect();

        let mut decoders = Vec::new();
        for x in 0..4 {
            for y in 0..2 {
                let decoder =
                    PngDecoder::new(File::open(blue_marble_directory.as_ref().join(format!(
                        "world.200406.3x21600x21600.{}{}.png",
                        "ABCD".chars().nth(x).unwrap(),
                        "12".chars().nth(y).unwrap()
                    )))?)?;
                assert_eq!(decoder.dimensions(), (bm_dimensions as u32, bm_dimensions as u32));
                assert_eq!(decoder.color_type(), ColorType::Rgb8);
                decoders.push(decoder.into_reader()?);
            }
        }

        let total = slices.len() / 8;
        for (i, chunk) in slices.chunks_mut(8).enumerate() {
            if i % 108 == 0 {
                progress_callback("Loading blue marble images... ", i / 108, total / 108);
            }

            decoders.par_iter_mut().zip(chunk).try_for_each(|(d, s)| d.read_exact(s))?;
        }

        let bluemarble =
            GlobalRaster { width: bm_dimensions * 4, height: bm_dimensions * 2, bands: 3, values };

        let mapfile = &self.mapfile;
        let progress = &Mutex::new((total_tiles - missing.len(), progress_callback));

        missing.into_par_iter().try_for_each(|n| -> Result<(), Error> {
            {
                let mut progress = progress.lock().unwrap();
                let v = progress.0;
                progress.1("Generating albedo... ", v, total_tiles);
                progress.0 += 1;
            }

            let mut colormap = Vec::with_capacity(
                layer.texture_resolution as usize * layer.texture_resolution as usize,
            );

            let coordinates: Vec<_> = (0..(layer.texture_resolution * layer.texture_resolution))
                .into_par_iter()
                .map(|i| {
                    let cspace = n.cell_position_cspace(
                        (i % layer.texture_resolution) as i32,
                        (i / layer.texture_resolution) as i32,
                        layer.texture_border_size as u16,
                        layer.texture_resolution as u16,
                    );
                    let polar = coordinates::cspace_to_polar(cspace);
                    (polar.x.to_degrees(), polar.y.to_degrees())
                })
                .collect();

            for (lat, long) in coordinates {
                colormap.extend_from_slice(&[
                    SRGB_TO_LINEAR[bluemarble.interpolate(lat, long, 0) as u8],
                    SRGB_TO_LINEAR[bluemarble.interpolate(lat, long, 1) as u8],
                    SRGB_TO_LINEAR[bluemarble.interpolate(lat, long, 2) as u8],
                    255,
                ]);
            }

            let mut data = Vec::new();
            let encoder = image::codecs::png::PngEncoder::new(&mut data);
            encoder.encode(
                &colormap,
                layer.texture_resolution as u32,
                layer.texture_resolution as u32,
                image::ColorType::Rgba8,
            )?;
            mapfile.write_tile(LayerType::Albedo, n, &data, true)
        })
    }

    pub async fn generate_roughness<F: FnMut(&str, usize, usize) + Send>(
        &mut self,
        mut progress_callback: F,
    ) -> Result<(), Error> {
        let (missing, total_tiles) = self.mapfile.get_missing_base(LayerType::Roughness)?;
        if missing.is_empty() {
            return Ok(());
        }

        let layer = self.mapfile.layers()[LayerType::Roughness].clone();
        assert!(layer.texture_border_size >= 2);
        assert_eq!(layer.texture_resolution % 4, 0);

        let total_missing = missing.len();
        for (i, n) in missing.into_iter().enumerate() {
            progress_callback(
                "Generating roughness... ",
                i + (total_tiles - total_missing),
                total_tiles,
            );

            let mut data = Vec::with_capacity(
                layer.texture_resolution as usize * layer.texture_resolution as usize / 2,
            );
            for _ in 0..(layer.texture_resolution / 4) {
                for _ in 0..(layer.texture_resolution / 4) {
                    data.extend_from_slice(&[179, 180, 0, 0, 0, 0, 0, 0]);
                }
            }

            let mut e = lz4::EncoderBuilder::new().level(9).build(Vec::new())?;
            e.write_all(&data)?;

            self.mapfile.write_tile(LayerType::Roughness, n, &e.finish().0, true)?;
        }

        Ok(())
    }
}

fn generate_noise(mapfile: &mut MapFile, context: &mut AssetLoadContext) -> Result<(), Error> {
    if !mapfile.reload_texture("noise") {
        // wavelength = 1.0 / 256.0;
        let noise_desc = TextureDescriptor {
            width: 2048,
            height: 2048,
            depth: 1,
            format: TextureFormat::RGBA8,
            bytes: 4 * 2048 * 2048,
        };

        let noise_heightmaps: Vec<_> =
            (0..4).map(|i| crate::terrain::heightmap::wavelet_noise(64 << i, 32 >> i)).collect();

        context.reset("Generating noise textures... ", noise_heightmaps.len());

        let len = noise_heightmaps[0].heights.len();
        let mut heights = vec![0u8; len * 4];
        for (i, heightmap) in noise_heightmaps.into_iter().enumerate() {
            context.set_progress(i as u64);
            let mut dist: Vec<(usize, f32)> = heightmap.heights.into_iter().enumerate().collect();
            dist.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
            for j in 0..len {
                heights[dist[j].0 * 4 + i] = (j * 256 / len) as u8;
            }
        }

        mapfile.write_texture("noise", noise_desc, &heights[..])?;
    }
    Ok(())
}

fn generate_sky(mapfile: &mut MapFile, context: &mut AssetLoadContext) -> Result<(), Error> {
    if !mapfile.reload_texture("sky") {
        context.reset("Generating sky texture... ", 1);
        let sky = WebTextureAsset {
            url: "https://www.eso.org/public/archives/images/original/eso0932a.tif".to_owned(),
            filename: "eso0932a.tif".to_owned(),
        }
        .load(context)?;
        mapfile.write_texture("sky", sky.0, &sky.1)?;
    }
    if !mapfile.reload_texture("transmittance") || !mapfile.reload_texture("inscattering") {
        let atmosphere = crate::sky::Atmosphere::new(context)?;
        mapfile.write_texture(
            "transmittance",
            TextureDescriptor {
                width: atmosphere.transmittance.size[0] as u32,
                height: atmosphere.transmittance.size[1] as u32,
                depth: 1,
                format: TextureFormat::RGBA32F,
                bytes: atmosphere.transmittance.data.len() * 4,
            },
            bytemuck::cast_slice(&atmosphere.transmittance.data),
        )?;
        mapfile.write_texture(
            "inscattering",
            TextureDescriptor {
                width: atmosphere.inscattering.size[0] as u32,
                height: atmosphere.inscattering.size[1] as u32,
                depth: atmosphere.inscattering.size[2] as u32,
                format: TextureFormat::RGBA32F,
                bytes: atmosphere.inscattering.data.len() * 4,
            },
            bytemuck::cast_slice(&atmosphere.inscattering.data),
        )?;
    }
    Ok(())
}

struct WebTextureAsset {
    url: String,
    filename: String,
}
impl WebAsset for WebTextureAsset {
    type Type = (TextureDescriptor, Vec<u8>);

    fn url(&self) -> String {
        self.url.clone()
    }
    fn filename(&self) -> String {
        self.filename.clone()
    }
    fn parse(&self, _context: &mut AssetLoadContext, data: Vec<u8>) -> Result<Self::Type, Error> {
        // TODO: handle other pixel formats
        let img = image::load_from_memory(&data)?.into_rgba8();
        Ok((
            TextureDescriptor {
                format: TextureFormat::RGBA8,
                width: img.width(),
                height: img.height(),
                depth: 1,
                bytes: (*img).len(),
            },
            img.into_raw(),
        ))
    }
}
