use std::error::Error;
use std::io::{Cursor, Read};

use gfx;
use gfx::format::*;
use gfx_core;
use image::{self, GenericImage};
use zip::ZipArchive;

use cache::{AssetLoadContext, GeneratedAsset, WebAsset};

#[derive(Clone, Copy)]
enum MaterialType {
    Rock = 0,
    Grass = 1,
}

struct MaterialTypeRaw(MaterialType);
impl WebAsset for MaterialTypeRaw {
    type Type = MaterialRaw;

    fn url(&self) -> String {
        match self.0 {
            MaterialType::Rock => "https://opengameart.org/sites/default/files/terrain_0.zip",
            MaterialType::Grass => "https://opengameart.org/sites/default/files/terrain_0.zip",
        }.to_owned()
    }

    fn filename(&self) -> String {
        let name = match self.0 {
            MaterialType::Rock => "terrain.zip",
            MaterialType::Grass => "terrain.zip",
        };
        format!("materials/raw/{}", name)
    }

    fn parse(
        &self,
        _context: &mut AssetLoadContext,
        data: Vec<u8>,
    ) -> Result<Self::Type, Box<Error>> {
        let name = match self.0 {
            MaterialType::Rock => "ground_mud2_d.jpg",
            MaterialType::Grass => "grass_ground_d.jpg",
        };

        let mut raw = MaterialRaw::default();
        let mut zip = ZipArchive::new(Cursor::new(data))?;
        for i in 0..zip.len() {
            let mut file = zip.by_index(i)?;
            if file.name().contains(name) {
                raw.albedo.clear();
                file.read_to_end(&mut raw.albedo)?;
            }
        }
        Ok(raw)
    }
}

impl GeneratedAsset for MaterialType {
    type Type = Material;

    fn filename(&self) -> String {
        let name = match *self {
            MaterialType::Rock => "rock.bin",
            MaterialType::Grass => "grass.bin",
        };
        format!("materials/filtered/{}", name)
    }

    fn generate(&self, context: &mut AssetLoadContext) -> Result<Self::Type, Box<Error>> {
        context.set_progress_and_total(0, 7);

        let resolution = 1024;
        let mipmaps = 11;

        let raw = MaterialTypeRaw(*self).load(context)?;
        let mut albedo_image =
            image::DynamicImage::ImageRgba8(image::load_from_memory(&raw.albedo[..])?.to_rgba());
        if albedo_image.width() != resolution || albedo_image.height() != resolution {
            albedo_image =
                albedo_image.resize_exact(resolution, resolution, image::FilterType::Triangle);
        }

        let albedo_image_blurred = {
            let sigma = 32;
            context.set_progress(1);
            let tiled =
                image::RgbaImage::from_fn(resolution + 4 * sigma, resolution + 4 * sigma, |x, y| {
                    albedo_image.get_pixel(
                        (x + resolution - 2 * sigma) % resolution,
                        (y + resolution - 2 * sigma) % resolution,
                    )
                });
            context.set_progress(2);
            let mut tiled = image::DynamicImage::ImageRgba8(tiled).blur(sigma as f32);
            context.set_progress(3);
            tiled.crop(2 * sigma, 2 * sigma, resolution, resolution)
        };

        context.set_progress(4);
        let mut albedo_sum = [0u64; 4];
        for (_, _, color) in albedo_image.pixels() {
            for i in 0..4 {
                albedo_sum[i] += color[i] as u64;
            }
        }
        let num_pixels = (albedo_image.width() * albedo_image.height()) as u64;
        let average_albedo: [u8; 4] = [
            (albedo_sum[0] / num_pixels) as u8,
            (albedo_sum[1] / num_pixels) as u8,
            (albedo_sum[2] / num_pixels) as u8,
            (albedo_sum[3] / num_pixels) as u8,
        ];

        context.set_progress(5);
        for (x, y, blurred_color) in albedo_image_blurred.pixels() {
            let mut color = albedo_image.get_pixel(x, y);
            for i in 0..4 {
                use image::math::utils::clamp;
                color[i] = clamp(
                    (color[i] as i16) - (blurred_color[i] as i16) +
                        (average_albedo[i] as i16),
                    0,
                    255,
                ) as u8;
            }
            albedo_image.put_pixel(x, y, color);
        }

        context.set_progress(6);
        let mut albedo = Vec::new();
        for level in 0..mipmaps {
            let level_resolution = (resolution >> level) as u32;
            if albedo_image.width() != level_resolution ||
                albedo_image.height() != level_resolution
            {
                albedo_image = albedo_image.resize_exact(
                    level_resolution,
                    level_resolution,
                    image::FilterType::Triangle,
                );
            }

            albedo.push(
                albedo_image.to_rgba().to_vec()[..]
                    .chunks(4)
                    .map(|c| [c[0], c[1], c[2], c[3]])
                    .collect(),
            );
        }
        context.set_progress(7);
        Ok(Material {
            resolution: resolution as u16,
            mipmaps,
            albedo,
        })
    }
}

/// Holds the raw bytes of the image files for each map of a material.
#[derive(Serialize, Deserialize, Default)]
struct MaterialRaw {
    albedo: Vec<u8>,
}

#[derive(Serialize, Deserialize)]
struct Material {
    resolution: u16,
    mipmaps: u8,

    albedo: Vec<Vec<[u8; 4]>>,
}

pub struct MaterialSet<R: gfx::Resources> {
    pub(crate) texture_view: gfx_core::handle::ShaderResourceView<R, [f32; 4]>,
    pub(crate) _texture: gfx_core::handle::Texture<R, gfx_core::format::R8_G8_B8_A8>,
    average_albedos: Vec<[u8; 4]>,
}

impl<R: gfx::Resources> MaterialSet<R> {
    pub fn load<F: gfx::Factory<R>, C: gfx_core::command::Buffer<R>>(
        factory: &mut F,
        encoder: &mut gfx::Encoder<R, C>,
    ) -> Result<Self, Box<Error>> {
        let resolution = 1024;
        let mipmaps = 11;

        let materials = vec![
            MaterialType::Rock.load(&mut AssetLoadContext::new())?,
            MaterialType::Grass.load(&mut AssetLoadContext::new())?,
        ];

        let mut average_albedos = Vec::new();

        let texture = factory
            .create_texture::<R8_G8_B8_A8>(
                gfx::texture::Kind::D2Array(
                    resolution,
                    resolution,
                    materials.len() as u16,
                    gfx::texture::AaMode::Single,
                ),
                mipmaps,
                gfx::memory::Bind::SHADER_RESOURCE,
                gfx::memory::Usage::Dynamic,
                Some(ChannelType::Srgb),
            )
            .unwrap();

        for (layer, material) in materials.iter().enumerate() {
            assert_eq!(mipmaps, material.mipmaps);
            assert_eq!(mipmaps as usize, material.albedo.len());

            for (level, albedo) in material.albedo.iter().enumerate() {
                encoder
                    .update_texture::<R8_G8_B8_A8, gfx::format::Srgba8>(
                        &texture,
                        None,
                        gfx_core::texture::NewImageInfo {
                            xoffset: 0,
                            yoffset: 0,
                            zoffset: layer as u16,
                            width: resolution >> level,
                            height: resolution >> level,
                            depth: 1,
                            format: (),
                            mipmap: level as u8,
                        },
                        &albedo[..],
                    )
                    .unwrap();
            }
            average_albedos.push(material.albedo.last().unwrap()[0]);
        }

        let texture_view = factory
            .view_texture_as_shader_resource::<gfx::format::Srgba8>(
                &texture,
                (0, mipmaps),
                Swizzle::new(),
            )
            .unwrap();

        Ok(Self {
            texture_view,
            _texture: texture,
            average_albedos,
        })
    }

    pub(crate) fn get_average_albedo(&self, material: usize) -> [u8; 4] {
        self.average_albedos[material].clone()
    }
}
