use crate::terrain::tile_cache::LayerType;
use vec_map::VecMap;

pub(crate) struct GpuState {
    pub noise: wgpu::Texture,
    pub _planet_mesh_texture: wgpu::Texture,

    pub tile_cache: VecMap<wgpu::Texture>,
}
impl GpuState {
    pub(crate) fn bind_group_for_shader(
        &self,
        device: &wgpu::Device,
        shader: &rshader::ShaderSet,
        ubo: Option<&wgpu::BindingResource>,
    ) -> (wgpu::BindGroup, wgpu::BindGroupLayout) {
        let linear = &device.create_sampler(&wgpu::SamplerDescriptor {
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Nearest,
            lod_min_clamp: -100.0,
            lod_max_clamp: 100.0,
            compare_function: wgpu::CompareFunction::Always,
        });
        let linear_wrap = &device.create_sampler(&wgpu::SamplerDescriptor {
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::Repeat,
            address_mode_w: wgpu::AddressMode::Repeat,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Nearest,
            lod_min_clamp: -100.0,
            lod_max_clamp: 100.0,
            compare_function: wgpu::CompareFunction::Always,
        });

        let noise = &self.noise.create_default_view();
        let tile_cache_views: VecMap<_> =
            self.tile_cache.iter().map(|(i, tex)| (i, tex.create_default_view())).collect();

        let bind_group_layout = device.create_bind_group_layout(&shader.layout_descriptor());
        let mut bindings = Vec::new();
        for (name, layout) in
            shader.desc_names().iter().zip(shader.layout_descriptor().bindings.iter())
        {
            let name = &**name.as_ref().unwrap();
            bindings.push(wgpu::Binding {
                binding: layout.binding,
                resource: match layout.ty {
                    wgpu::BindingType::Sampler => wgpu::BindingResource::Sampler(match name {
                        "linear" => &linear,
                        "linear_wrap" => &linear_wrap,
                        _ => unreachable!("unrecognized sampler: {}", name),
                    }),
                    wgpu::BindingType::UniformBuffer { .. } => ubo.cloned().unwrap(),
                    wgpu::BindingType::StorageTexture { .. }
                    | wgpu::BindingType::SampledTexture { .. } => {
                        wgpu::BindingResource::TextureView(match name {
                            "noise" => noise,
                            "displacements" => &tile_cache_views[LayerType::Displacements],
                            "normals" => &tile_cache_views[LayerType::Normals],
                            "albedo" => &tile_cache_views[LayerType::Albedo],
                            "heightmaps" => &tile_cache_views[LayerType::Heightmaps],
                            _ => unreachable!("unrecognized image: {}", name),
                        })
                    }
                    wgpu::BindingType::StorageBuffer { .. } => unimplemented!(),
                },
            })
        }

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            layout: &bind_group_layout,
            bindings: &*bindings,
        });

        (bind_group, bind_group_layout)
    }
}