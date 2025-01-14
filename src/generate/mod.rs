use crate::asset::{AssetLoadContext, AssetLoadContextBuf, WebAsset};
use crate::cache::{LayerParams, LayerType, TextureFormat};
use crate::coordinates;
use crate::generate::heightmap::{Sector, SectorCache};
use crate::mapfile::{MapFile, TextureDescriptor};
use crate::srgb::SRGB_TO_LINEAR;
use crate::terrain::raster::GlobalRaster;
use anyhow::Error;
use atomicwrites::{AtomicFile, OverwriteBehavior};
use basis_universal::Transcoder;
use fnv::FnvHashMap;
use futures::stream::FuturesUnordered;
use futures::{Future, StreamExt};
use image::{codecs::png::PngDecoder, ColorType, ImageDecoder};
use itertools::Itertools;
use rayon::prelude::*;
use std::collections::HashSet;
use std::io::Cursor;
use std::sync::atomic::AtomicUsize;
use std::{fs, mem};
use std::{fs::File, path::PathBuf};
use std::{
    io::{Read, Write},
    path::Path,
    sync::Mutex,
};
use types::{VFace, VNode};
use vec_map::VecMap;

mod gpu;
pub mod heightmap;
mod material;

pub(crate) use gpu::*;

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

pub(crate) struct MapFileBuilder(MapFile);
impl MapFileBuilder {
    pub(crate) async fn new() -> Self {
        let layers: VecMap<LayerParams> = LayerType::iter()
            .map(|layer_type| {
                let params = match layer_type {
                    LayerType::Heightmaps => LayerParams {
                        texture_resolution: 521,
                        texture_border_size: 4,
                        texture_format: &[TextureFormat::R32],
                        grid_registration: true,
                        min_level: 0,
                        max_level: VNode::LEVEL_CELL_5MM,
                        layer_type,
                    },
                    LayerType::Displacements => LayerParams {
                        texture_resolution: 65,
                        texture_border_size: 0,
                        texture_format: &[TextureFormat::RGBA32F],
                        grid_registration: true,
                        min_level: 0,
                        max_level: VNode::LEVEL_CELL_5MM,
                        layer_type,
                    },
                    LayerType::AlbedoRoughness => LayerParams {
                        texture_resolution: 516,
                        texture_border_size: 2,
                        texture_format: &[TextureFormat::RGBA8],
                        grid_registration: false,
                        min_level: 0,
                        max_level: VNode::LEVEL_CELL_5MM,
                        layer_type,
                    },
                    LayerType::Normals => LayerParams {
                        texture_resolution: 516,
                        texture_border_size: 2,
                        texture_format: &[TextureFormat::RG8],
                        grid_registration: false,
                        min_level: 0,
                        max_level: VNode::LEVEL_CELL_5MM,
                        layer_type,
                    },
                    LayerType::GrassCanopy => LayerParams {
                        texture_resolution: 516,
                        texture_border_size: 2,
                        texture_format: &[TextureFormat::RGBA8],
                        grid_registration: false,
                        min_level: VNode::LEVEL_CELL_1M,
                        max_level: VNode::LEVEL_CELL_1M,
                        layer_type,
                    },
                    LayerType::AerialPerspective => LayerParams {
                        texture_resolution: 17,
                        texture_border_size: 0,
                        texture_format: &[TextureFormat::RGBA16F],
                        grid_registration: true,
                        min_level: 3,
                        max_level: VNode::LEVEL_SIDE_610M,
                        layer_type,
                    },
                    LayerType::BentNormals => LayerParams {
                        texture_resolution: 513,
                        texture_border_size: 0,
                        texture_format: &[TextureFormat::RGBA8],
                        grid_registration: true,
                        min_level: VNode::LEVEL_CELL_153M,
                        max_level: VNode::LEVEL_CELL_76M,
                        layer_type,
                    },
                    LayerType::TreeCover => LayerParams {
                        texture_resolution: 516,
                        texture_border_size: 2,
                        texture_format: &[TextureFormat::R8],
                        grid_registration: false,
                        min_level: 0,
                        max_level: VNode::LEVEL_CELL_76M,
                        layer_type,
                    },
                    LayerType::BaseAlbedo => LayerParams {
                        texture_resolution: 516,
                        texture_border_size: 2,
                        texture_format: &[TextureFormat::RGBA8],
                        grid_registration: false,
                        min_level: 0,
                        max_level: VNode::LEVEL_CELL_610M,
                        layer_type,
                    },
                    LayerType::TreeAttributes => LayerParams {
                        texture_resolution: 516,
                        texture_border_size: 2,
                        texture_format: &[TextureFormat::RGBA8],
                        grid_registration: false,
                        min_level: VNode::LEVEL_CELL_10M,
                        max_level: VNode::LEVEL_CELL_10M,
                        layer_type,
                    },
                    LayerType::RootAerialPerspective => LayerParams {
                        texture_resolution: 65,
                        texture_border_size: 0,
                        texture_format: &[TextureFormat::RGBA16F],
                        grid_registration: true,
                        min_level: 0,
                        max_level: 0,
                        layer_type,
                    },
                };
                (layer_type.index(), params)
            })
            .collect();

        let mapfile = MapFile::new(layers);
        for layer in LayerType::iter() {
            if layer.streamed_levels() > 0 {
                mapfile.reload_tile_states(layer).await.unwrap();
            }
        }

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

        download_cloudcover(&mut self.0, &mut context)?;
        download_ground_albedo(&mut self.0, &mut context)?;
        download_models(&mut context)?;

        Ok(self.0)
    }
}

const SECTORS_PER_SIDE: u32 = 65;

fn scan_directory(
    base: &Path,
    suffix: impl AsRef<Path>,
) -> Result<(PathBuf, HashSet<String>), Error> {
    let directory = base.join(suffix);
    fs::create_dir_all(&directory)?;

    let mut existing = HashSet::new();
    for entry in fs::read_dir(&directory)? {
        if let Ok(s) = entry?.file_name().into_string() {
            existing.insert(s);
        }
    }

    Ok((directory, existing))
}

pub(crate) fn reproject_dataset<T, C, F, Downsample>(
    base_directory: PathBuf,
    dataset_name: &'static str,
    max_level: u8,
    progress_callback: F,
    grid_registration: bool,
    vrt_file: vrt_file::VrtFile,
    downsample: &'static Downsample,
    no_data_value: T,
) -> Result<(), anyhow::Error>
where
    T: vrt_file::Scalar + Ord + Copy + bytemuck::Pod + Send + Sync + 'static + From<i16>,
    F: FnMut(String, usize, usize) + Send,
    Downsample: Fn(T, T, T, T) -> T + Sync + 'static,
    C: tiff::encoder::colortype::ColorType<Inner = T>,
    [T]: tiff::encoder::TiffValue,
{
    let (reprojected_directory, reprojected) =
        scan_directory(&base_directory, format!("{}_reprojected", dataset_name))?;

    let mut missing = Vec::new();
    for root_node in VNode::roots() {
        for y in 0..SECTORS_PER_SIDE {
            for x in 0..SECTORS_PER_SIDE {
                let is_missing = (VNode::LEVEL_CELL_1KM.min(max_level)..=max_level).any(|level| {
                    !reprojected.contains(&format!(
                        "{}_S-{}-{:02}x{:02}.tiff",
                        VFace(root_node.face()),
                        level,
                        x,
                        y
                    ))
                });

                if is_missing {
                    missing.push((root_node, x, y));
                }
            }
        }
    }

    let min_level = VNode::LEVEL_CELL_1KM.min(max_level);

    const TILE_RESOLUTION: usize = 516;
    const BORDER_SIZE: usize = 2;
    const TILE_INNER_RESOLUTION: usize = TILE_RESOLUTION - BORDER_SIZE * 2;

    let base_sector_resolution = if grid_registration {
        1 + (TILE_INNER_RESOLUTION << max_level) as u32 / (SECTORS_PER_SIDE - 1)
    } else {
        (TILE_INNER_RESOLUTION << max_level) as u32 / (SECTORS_PER_SIDE - 1)
    };
    let root_border_size = base_sector_resolution / 2;

    base_sector_resolution
        .checked_mul(base_sector_resolution)
        .expect("TODO: Handle sector resolution overflow");

    let total_sectors = (6 * SECTORS_PER_SIDE * SECTORS_PER_SIDE) as usize;
    let sectors_processed = AtomicUsize::new(total_sectors - missing.len());

    let progress_callback = Mutex::new(progress_callback);
    let geotransform = vrt_file.geotransform();

    vrt_file.alloc_user_bytes(
        u64::from(base_sector_resolution * base_sector_resolution)
            * (16 + mem::size_of::<T>()) as u64
            * 16,
    );
    missing.chunks(16).try_for_each(|chunk| {
        chunk.into_par_iter().try_for_each(|(root, x, y)| -> Result<(), anyhow::Error> {
            (progress_callback.lock().unwrap())(
                format!("reprojecting {}...", dataset_name),
                sectors_processed.load(std::sync::atomic::Ordering::SeqCst),
                total_sectors,
            );

            let mut coordinates =
                Vec::with_capacity((base_sector_resolution * base_sector_resolution) as usize);
            if grid_registration {
                (0..(base_sector_resolution * base_sector_resolution))
                    .into_par_iter()
                    .map(|i| {
                        let cspace = root.grid_position_cspace(
                            (x * (base_sector_resolution - 1) + (i % base_sector_resolution))
                                as i32,
                            (y * (base_sector_resolution - 1) + (i / base_sector_resolution))
                                as i32,
                            root_border_size as u32,
                            ((base_sector_resolution - 1) * SECTORS_PER_SIDE + 1) as u32,
                        );
                        let polar = coordinates::cspace_to_polar(cspace);
                        let latitude = polar.x.to_degrees();
                        let longitude = polar.y.to_degrees();
                        let x = (longitude - geotransform[0]) / geotransform[1];
                        let y = (latitude - geotransform[3]) / geotransform[5];
                        (x, y)
                    })
                    .collect_into_vec(&mut coordinates);
            } else {
                (0..(base_sector_resolution * base_sector_resolution))
                    .into_par_iter()
                    .map(|i| {
                        let cspace = root.cell_position_cspace(
                            (x * base_sector_resolution + (i % base_sector_resolution)) as i32,
                            (y * base_sector_resolution + (i / base_sector_resolution)) as i32,
                            root_border_size as u32,
                            base_sector_resolution * SECTORS_PER_SIDE,
                        );
                        let polar = coordinates::cspace_to_polar(cspace);
                        let latitude = polar.x.to_degrees();
                        let longitude = polar.y.to_degrees();
                        let x = (longitude - geotransform[0]) / geotransform[1];
                        let y = (latitude - geotransform[3]) / geotransform[5];
                        (x, y)
                    })
                    .collect_into_vec(&mut coordinates);
            }

            let reprojected_directory = reprojected_directory.clone();

            let resolution = base_sector_resolution as usize;
            let mut heightmap = vec![no_data_value; resolution * resolution];

            vrt_file.batch_lookup(&*coordinates, &mut heightmap);

            drop(coordinates);

            let mut output_files = Vec::new();

            let mut resolution = base_sector_resolution;
            let mut downsampled: Vec<T> = heightmap.clone();
            for level in (min_level..=max_level).rev() {
                let mut bytes = Vec::new();

                let mut min = downsampled[0];
                let mut max = downsampled[0];
                for &v in &downsampled {
                    min = min.min(v);
                    max = max.max(v);
                }
                if min == max {
                    tiff::encoder::TiffEncoder::new(std::io::Cursor::new(&mut bytes))?
                        .write_image::<C>(1, 1, &[min])?;
                } else {
                    tiff::encoder::TiffEncoder::new(std::io::Cursor::new(&mut bytes))?
                        .write_image_with_compression::<C, _>(
                            resolution as u32,
                            resolution as u32,
                            tiff::encoder::compression::Lzw,
                            &downsampled,
                        )?;
                }

                let filename = reprojected_directory.join(&format!(
                    "{}_S-{}-{:02}x{:02}.tiff",
                    VFace(root.face()),
                    level,
                    x,
                    y
                ));
                output_files.push((filename, bytes));

                if level != min_level {
                    if grid_registration {
                        let half_resolution = (resolution - 1) / 2 + 1;
                        let mut half =
                            vec![no_data_value; (half_resolution * half_resolution) as usize];
                        for y in 0..half_resolution {
                            for x in 0..half_resolution {
                                half[(y * half_resolution + x) as usize] =
                                    downsampled[(y * 2 * resolution + x * 2) as usize];
                            }
                        }
                        downsampled = half;
                        resolution = half_resolution;
                    } else {
                        let half_resolution = resolution / 2;
                        let mut half =
                            vec![no_data_value; (half_resolution * half_resolution) as usize];
                        for y in 0..half_resolution {
                            for x in 0..half_resolution {
                                let (x2, y2) = (x * 2, y * 2);
                                half[(y * half_resolution + x) as usize] = downsample(
                                    downsampled[(y2 * resolution + x2) as usize],
                                    downsampled[((y2 + 1) * resolution + x2) as usize],
                                    downsampled[(y2 * resolution + x2 + 1) as usize],
                                    downsampled[((y2 + 1) * resolution + x2 + 1) as usize],
                                );
                            }
                        }
                        downsampled = half;
                        resolution = half_resolution;
                    }
                }
            }

            for (filename, bytes) in output_files.into_iter().rev() {
                AtomicFile::new(filename, OverwriteBehavior::AllowOverwrite)
                    .write(|f| f.write_all(&bytes))?;
            }

            sectors_processed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        })
    })?;
    vrt_file.free_user_bytes(
        u64::from(base_sector_resolution * base_sector_resolution)
            * (16 + mem::size_of::<T>()) as u64
            * 16,
    );
    Ok(())
}

pub(crate) fn merge_datasets_to_tiles<T, C, F, Downsample, FromF64>(
    base_directory: PathBuf,
    dataset_name: &'static str,
    max_level: u8,
    mut progress_callback: F,
    grid_registration: bool,
) -> impl Future<Output = Result<(), anyhow::Error>>
where
    T: Into<f64> + num_traits::Zero + Ord + Copy + bytemuck::Pod + Send + Sync + 'static,
    F: FnMut(&str, usize, usize) + Send,
    Downsample: Fn(T, T, T, T) -> T + Sync + 'static,
    FromF64: Fn(f64) -> T + Sync + 'static,
    C: tiff::encoder::colortype::ColorType<Inner = T>,
    [T]: tiff::encoder::TiffValue,
{
    async move {
        let (reprojected_directory, _reprojected) =
            scan_directory(&base_directory, format!("{}_reprojected", dataset_name))?;
        let (tiles_directory, existing_tiles) =
            scan_directory(&base_directory, format!("tiles/{}", dataset_name))?;

        let min_level = VNode::LEVEL_CELL_1KM.min(max_level);

        const TILE_RESOLUTION: usize = 516;
        const BORDER_SIZE: usize = 2;
        const TILE_INNER_RESOLUTION: usize = TILE_RESOLUTION - BORDER_SIZE * 2;

        let base_sector_resolution = if grid_registration {
            1 + (TILE_INNER_RESOLUTION << max_level) as u32 / (SECTORS_PER_SIDE - 1)
        } else {
            (TILE_INNER_RESOLUTION << max_level) as u32 / (SECTORS_PER_SIDE - 1)
        };

        base_sector_resolution
            .checked_mul(base_sector_resolution)
            .expect("TODO: Handle sector resolution overflow");

        const MAX_CONCURRENT: usize = 1;
        const MAX_RASTERS: usize = 8;

        let mut total_tiles = 0;
        let mut missing_tiles = Vec::new();
        VNode::breadth_first(|n| {
            let filename = format!(
                "{}_{}_{}_{}x{}.tiff",
                dataset_name,
                n.level(),
                VFace(n.face()),
                n.x(),
                n.y()
            );

            total_tiles += 1;
            if !existing_tiles.contains(&filename) {
                missing_tiles.push((tiles_directory.join(filename), n));
            }

            n.level() < max_level
        });
        missing_tiles.reverse();

        let mut sector_cache =
            SectorCache::new(32, reprojected_directory.to_owned(), "", "tiff", &|bytes| -> Result<
                Vec<T>,
                _,
            > {
                let mut decoder = tiff::decoder::Decoder::new(Cursor::new(bytes))?;
                Ok(match decoder.read_image()? {
                    tiff::decoder::DecodingResult::U8(v) => bytemuck::cast_vec(v),
                    _ => unimplemented!(),
                })
            });
        let mut unordered = FuturesUnordered::new();
        let mut tiles_processed = total_tiles - missing_tiles.len();
        while !missing_tiles.is_empty() || !unordered.is_empty() {
            if unordered.len() < 16 && !missing_tiles.is_empty() {
                let (filename, node) = missing_tiles.pop().unwrap();

                let mut heights = vec![T::zero(); TILE_RESOLUTION * TILE_RESOLUTION];

                let step = 1 << min_level.saturating_sub(node.level());
                let sector_level = node.level().max(min_level);
                let sector_inner_resolution =
                    (512 << sector_level) / (SECTORS_PER_SIDE - 1) as usize;
                let sector_resolution = if grid_registration {
                    1 + sector_inner_resolution
                } else {
                    sector_inner_resolution
                };
                let root_x = node.x() as usize * TILE_INNER_RESOLUTION * step
                    + sector_resolution / 2
                    - BORDER_SIZE * step;
                let root_y = node.y() as usize * TILE_INNER_RESOLUTION * step
                    + sector_resolution / 2
                    - BORDER_SIZE * step;

                let mut sectors = FnvHashMap::default();

                let min_sector_x = (root_x / sector_inner_resolution) as u32;
                let min_sector_y = (root_y / sector_inner_resolution) as u32;
                let max_sector_x =
                    ((root_x + (TILE_RESOLUTION - 1) * step) / sector_inner_resolution) as u32;
                let max_sector_y =
                    ((root_y + (TILE_RESOLUTION - 1) * step) / sector_inner_resolution) as u32;
                for y in min_sector_y..=max_sector_y {
                    for x in min_sector_x..=max_sector_x {
                        let s = Sector { face: node.face(), x, y };
                        if !sectors.contains_key(&s) {
                            sectors.insert(s, sector_cache.get_sector(s, Some(sector_level)));
                        }
                    }
                }

                // for y in (0..TILE_RESOLUTION).step_by(2) {
                //     for x in (0..TILE_RESOLUTION).step_by(2) {
                //         let s = Sector {
                //             face: node.face(),
                //             x: ((x * step + root_x) / sector_inner_resolution) as u32,
                //             y: ((y * step + root_y) / sector_inner_resolution) as u32,
                //         };
                //         if !sectors.contains_key(&s) {
                //             //eprintln!("x={}, y={}, step={}, root_x={}, root_y={}", x, y, step, root_x, root_y);
                //             sectors.insert(s, sector_cache.get_sector(s, Some(sector_level)));
                //         }
                //     }
                // }

                unordered.push(async move {
                    let sectors: Vec<(Sector, Result<_, _>)> = futures::future::join_all(
                        sectors.into_iter().map(|s| async move { (s.0, s.1.await) }),
                    )
                    .await;

                    let mut sectors_map = FnvHashMap::default();
                    for s in sectors {
                        sectors_map.insert(s.0, s.1?);
                    }

                    let encoded = tokio::task::spawn_blocking(move || {
                        for y in 0..TILE_RESOLUTION {
                            for x in 0..TILE_RESOLUTION {
                                let s = Sector {
                                    face: node.face(),
                                    x: ((x * step + root_x) / sector_inner_resolution) as u32,
                                    y: ((y * step + root_y) / sector_inner_resolution) as u32,
                                };
                                let sector = &sectors_map[&s];
                                if sector.len() == 1 {
                                    heights[y * TILE_RESOLUTION + x] = sector[0];
                                } else {
                                    let sector_x = (x * step + root_x) % sector_inner_resolution;
                                    let sector_y = (y * step + root_y) % sector_inner_resolution;

                                    heights[y * TILE_RESOLUTION + x] =
                                        sector[sector_y * sector_resolution + sector_x];
                                }
                            }
                        }

                        let mut bytes = Vec::new();
                        if heights.iter().any(|&h| h != T::zero()) {
                            tiff::encoder::TiffEncoder::new(std::io::Cursor::new(&mut bytes))?
                                .write_image_with_compression::<C, _>(
                                    TILE_RESOLUTION as u32,
                                    TILE_RESOLUTION as u32,
                                    tiff::encoder::compression::Lzw,
                                    &heights,
                                )?;
                            // } else {
                            //     tiff::encoder::TiffEncoder::new(std::io::Cursor::new(&mut bytes))?
                            //         .write_image_with_compression::<C, _>(
                            //             1,
                            //             1,
                            //             tiff::encoder::compression::Lzw,
                            //             &heights[..1],
                            //         )?;
                        }

                        Ok::<_, anyhow::Error>((filename, bytes))
                    })
                    .await?;

                    Ok::<_, anyhow::Error>(encoded)
                })
            } else {
                let (filename, bytes) = unordered.next().await.unwrap()??;

                AtomicFile::new(filename, OverwriteBehavior::AllowOverwrite)
                    .write(|f| f.write_all(&bytes))?;

                tiles_processed += 1;
                progress_callback(
                    &format!("Generating {} tiles...", dataset_name),
                    tiles_processed,
                    total_tiles,
                );
            }
        }

        Ok(())
    }
}

// /// Generate heightmap tiles.
// ///
// /// `etopo1_file` is the location of [ETOPO1_Ice_c_geotiff.zip](https://www.ngdc.noaa.gov/mgg/global/relief/ETOPO1/data/ice_surface/cell_registered/georeferenced_tiff/ETOPO1_Ice_c_geotiff.zip).
// pub(crate) async fn generate_heightmaps<F: FnMut(&str, usize, usize) + Send>(
//     mapfile: &MapFile,
//     etopo1_file: impl AsRef<Path>,
//     nasadem_directory: PathBuf,
//     nasadem_reprojected_directory: PathBuf,
//     mut progress_callback: F,
// ) -> Result<(), Error> {
//     let (missing_tiles, total_tiles) = mapfile.get_missing_base(LayerType::Heightmaps);
//     if missing_tiles.is_empty() {
//         return Ok(());
//     }

//     let base_level = VNode::LEVEL_CELL_38M;
//     let sector_size = 8;

//     let layer = &mapfile.layers()[LayerType::Heightmaps];
//     let resolution = layer.texture_resolution as usize;
//     let border_size = layer.texture_border_size as usize;
//     let sectors_per_side = resolution / sector_size;

//     let sector_resolution = (sector_size << base_level) + 1;
//     let root_resolution = ((resolution - 1) << base_level) + 1;
//     let root_border_size = border_size << base_level;

//     assert_eq!((resolution - 1) % sector_size, 0);

//     // Scan the working directory to see what files already exist.
//     fs::create_dir_all(&nasadem_reprojected_directory)?;
//     let mut existing_files = HashSet::new();
//     for entry in fs::read_dir(&nasadem_reprojected_directory)? {
//         if let Ok(s) = entry?.file_name().into_string() {
//             existing_files.insert(s);
//         }
//     }

//     // See which sectors need to be generated.
//     let (missing_sectors, total_sectors) = {
//         let mut missing_sectors = Vec::new();
//         let mut total_sectors = 0;
//         for &root_node in &VNode::roots() {
//             for x in 0..sectors_per_side {
//                 for y in 0..sectors_per_side {
//                     total_sectors += 1;
//                     if !existing_files.contains(&format!(
//                         "nasadem_S-{}-{}x{}.raw",
//                         VFace(root_node.face() as u8),
//                         x,
//                         y
//                     )) {
//                         missing_sectors.push((root_node, x, y));
//                     }
//                 }
//             }
//         }
//         (missing_sectors, total_sectors)
//     };

//     // Generate missing sectors.
//     if !missing_sectors.is_empty() {
//         let mut gen = heightmap::HeightmapSectorGen {
//             sector_resolution,
//             root_resolution,
//             root_border_size,
//             dems: crate::terrain::dem::make_nasadem_raster_cache(&nasadem_directory, 64),
//             global_dem: Arc::new(crate::terrain::dem::parse_etopo1(
//                 etopo1_file,
//                 &mut progress_callback,
//             )?),
//         };

//         const MAX_CONCURRENT: usize = 32;
//         const MAX_RASTERS: usize = 256;

//         let mut sectors_processed = total_sectors - missing_sectors.len();
//         let mut missing = missing_sectors.into_iter().peekable();
//         let mut pending = futures::stream::FuturesUnordered::new();

//         let mut loaded_rasters = 0;
//         let mut unstarted: Option<(usize, BoxFuture<_>)> = None;

//         loop {
//             progress_callback("Generating heightmap sectors...", sectors_processed, total_sectors);
//             if unstarted.is_some()
//                 && (loaded_rasters + unstarted.as_ref().unwrap().0 <= MAX_RASTERS
//                     || loaded_rasters == 0)
//             {
//                 let (num_rasters, future) = unstarted.take().unwrap();
//                 loaded_rasters += num_rasters;
//                 pending.push(future);
//             } else if unstarted.is_none()
//                 && pending.len() < MAX_CONCURRENT
//                 && missing.peek().is_some()
//             {
//                 let (root_node, x, y) = missing.next().unwrap();
//                 unstarted = Some(gen.generate_sector(
//                     root_node,
//                     x,
//                     y,
//                     nasadem_reprojected_directory.join(&format!(
//                         "nasadem_S-{}-{}x{}.raw",
//                         VFace(root_node.face()),
//                         x,
//                         y
//                     )),
//                 ));
//             } else {
//                 match pending.next().await {
//                     Some(result) => {
//                         loaded_rasters -= result?;
//                         sectors_processed += 1;
//                     }
//                     None => break,
//                 }
//             }
//         }
//     }

//     // See which faces need to be generated.
//     let face_resolution = sectors_per_side * 512 + 1;
//     for face in 0..6 {
//         //progress_callback("Generating heightmap faces...", face as usize, 6);
//         let face_filename =
//             format!("nasadem_F-{}-{}x{}.tiff", VFace(face), face_resolution, face_resolution);
//         if existing_files.contains(&face_filename) {
//             continue;
//         }

//         let mut heightmap = vec![0i16; face_resolution * face_resolution];
//         for y in 0..sectors_per_side {
//             progress_callback(
//                 "Downsampling sectors...",
//                 face as usize * sectors_per_side * sectors_per_side + y * sectors_per_side,
//                 6 * sectors_per_side * sectors_per_side,
//             );

//             let mut unordered = FuturesUnordered::new();
//             for x in 0..sectors_per_side {
//                 let path = nasadem_reprojected_directory.join(&format!(
//                     "nasadem_S-{}-{}x{}.raw",
//                     VFace(face),
//                     x,
//                     y
//                 ));
//                 unordered.push(async move {
//                     let bytes = tokio::fs::read(path).await?;
//                     let tile = tokio::task::spawn_blocking(move || {
//                         let (sector_resolution, sector) =
//                             tilefmt::uncompress_heightmap_tile(None, &bytes);

//                         let step = ((sector_resolution - 1) * (resolution / sector_size))
//                             / (face_resolution - 1);

//                         let downsampled_resolution = (sector_resolution / step) + 1;
//                         let mut downsampled =
//                             vec![0; downsampled_resolution * downsampled_resolution];

//                         for y in 0..downsampled_resolution {
//                             for x in 0..downsampled_resolution {
//                                 downsampled[y * downsampled_resolution + x] =
//                                     sector[y * step * sector_resolution + x * step];
//                             }
//                         }

//                         (downsampled_resolution, downsampled)
//                     })
//                     .await?;

//                     Ok::<_, Error>((x, tile))
//                 });
//             }

//             while !unordered.is_empty() {
//                 let (x, (downsampled_resolution, downsampled)) = unordered.next().await.unwrap()?;

//                 let origin_x = x * (face_resolution - 1) / sectors_per_side;
//                 let origin_y = y * (face_resolution - 1) / sectors_per_side;

//                 for k in 0..downsampled_resolution {
//                     heightmap[(origin_y + k) * face_resolution + origin_x..]
//                         [..downsampled_resolution]
//                         .copy_from_slice(
//                             &downsampled[k * downsampled_resolution..][..downsampled_resolution],
//                         );
//                 }
//             }
//         }

//         let mut bytes = Vec::new();
//         tiff::encoder::TiffEncoder::new(std::io::Cursor::new(&mut bytes))?
//             .write_image::<tiff::encoder::colortype::GrayI16>(
//             face_resolution as u32,
//             face_resolution as u32,
//             &heightmap,
//         )?;

//         // let tile = heightmap::compress_heightmap_tile(face_resolution, 0, &heightmap, None, 9);
//         AtomicFile::new(
//             nasadem_reprojected_directory.join(&face_filename),
//             OverwriteBehavior::AllowOverwrite,
//         )
//         .write(|f| f.write_all(&bytes))?;
//     }

//     let mut tiles_processed = total_tiles - missing_tiles.len();
//     let mut missing_by_face = VecMap::new();
//     for m in missing_tiles {
//         missing_by_face.entry(m.face().into()).or_insert(Vec::new()).push(m);
//     }

//     let mut sector_cache = SectorCache::new(
//         32,
//         nasadem_reprojected_directory.to_owned(),
//         "nasadem",
//         "raw",
//         &|bytes| Ok(tilefmt::uncompress_heightmap_tile(None, bytes).1),
//     );
//     let mut tile_cache = HeightmapCache::new(resolution, border_size, 128);
//     for (face, mut missing) in missing_by_face {
//         missing.sort_by_key(|m| m.level());
//         missing.reverse();
//         if missing.is_empty() {
//             continue;
//         }

//         let face_heightmap = {
//             let face_filename = format!(
//                 "nasadem_F-{}-{}x{}.tiff",
//                 VFace(face as u8),
//                 face_resolution,
//                 face_resolution
//             );
//             let bytes = tokio::fs::read(nasadem_reprojected_directory.join(face_filename)).await?;
//             let mut limits = tiff::decoder::Limits::default();
//             limits.decoding_buffer_size = 4 << 30;
//             let mut decoder = tiff::decoder::Decoder::new(Cursor::new(&bytes))?.with_limits(limits);
//             if let tiff::decoder::DecodingResult::I16(v) = decoder.read_image()? {
//                 Arc::new(v)
//             } else {
//                 unreachable!()
//             }
//         };

//         let mut unordered = FuturesUnordered::new();
//         while !missing.is_empty() || !unordered.is_empty() {
//             if unordered.len() < 16 && !missing.is_empty() {
//                 let node = missing.pop().unwrap();
//                 let parent = node.parent().map(|p| (p.1, tile_cache.get_tile(&mapfile, p.0)));

//                 let mut heights = vec![0; resolution * resolution];
//                 let fut = if ((resolution - 1) << node.level() + 1) < face_resolution {
//                     let face_scale = (face_resolution - 1) / (resolution - 1);
//                     let face_step = face_scale >> node.level();
//                     let skirt = border_size * (face_scale - face_step);
//                     let face_x =
//                         skirt + node.x() as usize * (resolution - border_size * 2 - 1) * face_step;
//                     let face_y =
//                         skirt + node.y() as usize * (resolution - border_size * 2 - 1) * face_step;

//                     let face_heightmap = Arc::clone(&face_heightmap);

//                     async move {
//                         Ok::<_, anyhow::Error>(
//                             tokio::task::spawn_blocking(move || {
//                                 for y in 0..resolution {
//                                     for x in 0..resolution {
//                                         heights[y * resolution + x] = face_heightmap[(face_y
//                                             + y * face_step)
//                                             * face_resolution
//                                             + face_x
//                                             + x * face_step];
//                                     }
//                                 }
//                                 heights
//                             })
//                             .await?,
//                         )
//                     }
//                     .boxed()
//                 } else {
//                     let step = 1 << (base_level - node.level());
//                     let root_x = node.x() as usize * (resolution - border_size * 2 - 1) * step
//                         + root_border_size
//                         - border_size * step;
//                     let root_y = node.y() as usize * (resolution - border_size * 2 - 1) * step
//                         + root_border_size
//                         - border_size * step;

//                     let mut sectors = FnvHashMap::default();
//                     for y in (0..resolution).step_by(8) {
//                         for x in (0..resolution).step_by(8) {
//                             let s = Sector {
//                                 face: face as u8,
//                                 x: ((x * step + root_x) / (sector_resolution - 1)) as u32,
//                                 y: ((y * step + root_y) / (sector_resolution - 1)) as u32,
//                             };
//                             if !sectors.contains_key(&s) {
//                                 //eprintln!("x={}, y={}, step={}, root_x={}, root_y={}", x, y, step, root_x, root_y);
//                                 sectors.insert(s, sector_cache.get_sector(s, None));
//                             }
//                         }
//                     }

//                     async move {
//                         let sectors: Vec<(Sector, Result<_, _>)> = futures::future::join_all(
//                             sectors.into_iter().map(|s| async move { (s.0, s.1.await) }),
//                         )
//                         .await;

//                         let mut sectors_map = FnvHashMap::default();
//                         for s in sectors {
//                             sectors_map.insert(s.0, s.1?);
//                         }

//                         Ok::<_, anyhow::Error>(
//                             tokio::task::spawn_blocking(move || {
//                                 for y in 0..resolution {
//                                     for x in 0..resolution {
//                                         let sector_x =
//                                             (x * step + root_x) % (sector_resolution - 1);
//                                         let sector_y =
//                                             (y * step + root_y) % (sector_resolution - 1);

//                                         let s = Sector {
//                                             face: face as u8,
//                                             x: ((x * step + root_x) / (sector_resolution - 1))
//                                                 as u32,
//                                             y: ((y * step + root_y) / (sector_resolution - 1))
//                                                 as u32,
//                                         };
//                                         let sector = &sectors_map[&s];
//                                         heights[y * resolution + x] =
//                                             sector[sector_y * sector_resolution + sector_x];
//                                     }
//                                 }
//                                 heights
//                             })
//                             .await?,
//                         )
//                     }
//                     .boxed()
//                 };

//                 unordered.push(async move {
//                     let mut heights = fut.await?;
//                     heights.iter_mut().for_each(|h| {
//                         if *h < -512 {
//                             *h = -512;
//                         }
//                     });

//                     let parent = if let Some(p) = parent {
//                         let tile = p.1.await?;
//                         assert_eq!(tile.len(), resolution * resolution);
//                         Some((p.0, border_size, tile))
//                     } else {
//                         None
//                     };
//                     Ok::<_, anyhow::Error>(
//                         tokio::task::spawn_blocking(move || {
//                             let parent_heights;
//                             let parent = match parent {
//                                 Some(p) => {
//                                     parent_heights = p.2;
//                                     Some((NODE_OFFSETS[p.0 as usize], p.1, &**parent_heights))
//                                 }
//                                 None => None,
//                             };

//                             (
//                                 node,
//                                 tilefmt::compress_heightmap_tile(
//                                     resolution,
//                                     2 + VNode::LEVEL_CELL_76M.saturating_sub(node.level()) as i8,
//                                     &heights,
//                                     parent,
//                                     5,
//                                 ),
//                             )
//                         })
//                         .await?,
//                     )
//                 })
//             } else {
//                 let (node, bytes) = unordered.next().await.unwrap()?;
//                 mapfile.write_tile(LayerType::Heightmaps, node, &bytes)?;

//                 tiles_processed += 1;
//                 progress_callback("Generating heightmap tiles...", tiles_processed, total_tiles);
//             }
//         }
//     }

//     Ok(())
// }

/// Generate albedo tiles.
///
/// `blue_marble_directory` must contain the 8 files from NASA's Blue Marble: Next Generation
/// indicated in [`BLUE_MARBLE_URLS`](constant.BLUE_MARBLE_URLS.html).
pub(crate) async fn generate_albedos<F: FnMut(&str, usize, usize) + Send>(
    mapfile: &MapFile,
    blue_marble_directory: impl AsRef<Path>,
    mut progress_callback: F,
) -> Result<(), Error> {
    let (missing, total_tiles) = mapfile.get_missing_base(LayerType::BaseAlbedo);
    if missing.is_empty() {
        return Ok(());
    }

    let layer = mapfile.layers()[LayerType::BaseAlbedo].clone();
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

    let mapfile = &mapfile;
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
                    layer.texture_border_size,
                    layer.texture_resolution,
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
        mapfile.write_tile(LayerType::BaseAlbedo, n, &data)
    })
}

pub(crate) async fn generate_materials<F: FnMut(String, usize, usize) + Send>(
    mapfile: &MapFile,
    free_pbr_directory: PathBuf,
    mut progress_callback: F,
) -> Result<(), Error> {
    if mapfile.reload_texture("ground_albedo") {
        return Ok(());
    }

    let mut albedo_params = basis_universal::encoding::CompressorParams::new();
    albedo_params.set_basis_format(basis_universal::BasisTextureFormat::UASTC4x4);
    albedo_params.set_generate_mipmaps(true);

    let materials = [("ground", "leafy-grass2"), ("ground", "grass1"), ("rocks", "granite5")];

    for (i, (group, name)) in materials.iter().enumerate() {
        let path = free_pbr_directory.join(format!("Blender/{}-bl/{}-bl", group, name));

        let mut albedo_path = None;
        for file in std::fs::read_dir(&path)? {
            let file = file?;
            let filename = file.file_name();
            let filename = filename.to_string_lossy();
            if filename.contains("albedo") {
                albedo_path = Some(file.path());
            }
        }

        let mut albedo = image::open(albedo_path.unwrap())?.to_rgb8();
        //material::high_pass_filter(&mut albedo);
        assert_eq!(albedo.width(), 2048);
        assert_eq!(albedo.height(), 2048);

        albedo =
            image::imageops::resize(&albedo, 1024, 1024, image::imageops::FilterType::Triangle);

        albedo_params.source_image_mut(i as u32).init(&*albedo, 1024, 1024, 3);
    }

    progress_callback("Compressing ground albedo textures".to_owned(), 0, 1);
    let mut compressor = basis_universal::encoding::Compressor::new(8);
    unsafe { compressor.init(&albedo_params) };
    unsafe { compressor.process().unwrap() };
    progress_callback("Compressing ground albedo textures".to_owned(), 1, 1);

    let albedo_desc = TextureDescriptor {
        width: 1024,
        height: 1024,
        depth: materials.len() as u32,
        format: TextureFormat::UASTC,
        array_texture: true,
    };

    mapfile.write_texture("ground_albedo", albedo_desc, compressor.basis_file())?;

    Ok(())
}

fn generate_noise(mapfile: &mut MapFile, context: &mut AssetLoadContext) -> Result<(), Error> {
    if !mapfile.reload_texture("noise") {
        // wavelength = 1.0 / 256.0;
        let noise_desc = TextureDescriptor {
            width: 2048,
            height: 2048,
            depth: 1,
            format: TextureFormat::RGBA8,
            array_texture: false,
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
            format: TextureFormat::RGBA8,
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
                array_texture: false,
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
                array_texture: false,
            },
            bytemuck::cast_slice(&atmosphere.inscattering.data),
        )?;
    }
    Ok(())
}

fn download_cloudcover(mapfile: &mut MapFile, context: &mut AssetLoadContext) -> Result<(), Error> {
    if !mapfile.reload_texture("cloudcover") {
        let cloudcover = WebTextureAsset {
            url: "https://terra.fintelia.io/file/terra-tiles/clouds_combined.png".to_owned(),
            filename: "clouds_combined.png".to_owned(),
            format: TextureFormat::RGBA8,
        }
        .load(context)?;
        mapfile.write_texture("cloudcover", cloudcover.0, &cloudcover.1)?;
    }

    Ok(())
}

fn download_ground_albedo(
    mapfile: &mut MapFile,
    context: &mut AssetLoadContext,
) -> Result<(), Error> {
    if !mapfile.reload_texture("ground_albedo") {
        let texture = WebTextureAsset {
            url: "https://terra.fintelia.io/file/terra-tiles/ground_albedo.basis".to_owned(),
            filename: "ground_albedo.basis".to_owned(),
            format: TextureFormat::UASTC,
        }
        .load(context)?;
        mapfile.write_texture("ground_albedo", texture.0, &texture.1)?;
    }

    Ok(())
}

fn download_models(context: &mut AssetLoadContext) -> Result<(), Error> {
    WebModel {
        url: "https://terra.fintelia.io/file/terra-tiles/Oak_English_Sapling.zip".to_owned(),
        filename: "Oak_English_Sapling.zip".to_owned(),
    }
    .load(context)
}

struct WebTextureAsset {
    url: String,
    filename: String,
    format: TextureFormat,
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
        match self.format {
            TextureFormat::UASTC => {
                let transcoder = Transcoder::new();
                let depth = transcoder.image_count(&data);
                let info = transcoder.image_info(&data, 0).unwrap();
                Ok((
                    TextureDescriptor {
                        format: self.format,
                        width: info.m_width,
                        height: info.m_height,
                        depth,
                        array_texture: true,
                    },
                    data,
                ))
            }
            TextureFormat::RGBA8 => {
                let img = image::load_from_memory(&data)?.into_rgba8();
                Ok((
                    TextureDescriptor {
                        format: TextureFormat::RGBA8,
                        width: img.width(),
                        height: img.height(),
                        depth: 1,
                        array_texture: false,
                    },
                    img.into_raw(),
                ))
            }
            _ => unimplemented!(),
        }
    }
}

struct WebModel {
    url: String,
    filename: String,
}
impl WebAsset for WebModel {
    type Type = ();

    fn url(&self) -> String {
        self.url.clone()
    }
    fn filename(&self) -> String {
        self.filename.clone()
    }
    fn parse(&self, _context: &mut AssetLoadContext, _data: Vec<u8>) -> Result<(), Error> {
        Ok(())
    }
}
