extern crate camera_controllers;
extern crate cgmath;
extern crate fps_counter;
extern crate gfx;
extern crate gfx_text;
extern crate piston_window;
extern crate terra;
extern crate vecmath;

use std::time::Instant;

use fps_counter::FPSCounter;
use piston_window::*;
use camera_controllers::{model_view_projection, CameraPerspective, FirstPerson,
                         FirstPersonSettings};
use vecmath::traits::Sqrt;

use terra::{DemSource, MaterialSet, Skybox, TerrainFileParams, TextureQuality, VertexQuality};

fn main() {
    let mut window: PistonWindow = PistonWindow::new(
        OpenGL::V3_3,
        0,
        WindowSettings::new("terra preview", [1920 / 2, 1080 / 2])
            .exit_on_esc(true)
            .opengl(OpenGL::V3_3)
            .vsync(false)
            .srgb(false)
            .build()
            .unwrap(),
    );
    window.set_capture_cursor(true);
    window.set_max_fps(240);

    let materials = MaterialSet::load(&mut window.factory, &mut window.encoder).unwrap();
    window.encoder.flush(&mut window.device);

    let sky = Skybox::new(&mut window.factory, &mut window.encoder);

    let mut terrain = TerrainFileParams {
        latitude: 42,
        longitude: -73,
        source: DemSource::Srtm30m,
        vertex_quality: VertexQuality::Medium,
        texture_quality: TextureQuality::VeryLow,
        materials,
        sky,
    }.build_quadtree(
        window.factory.clone(),
        &window.output_color,
        &window.output_stencil,
    )
        .unwrap();

    let get_projection = |w: &PistonWindow| {
        let draw_size = w.window.draw_size();
        CameraPerspective {
            fov: 90.0 * 9.0 / 16.0,
            near_clip: 100.0,
            far_clip: 50000000.0,
            aspect_ratio: (draw_size.width as f32) / (draw_size.height as f32),
        }.projection()
    };

    let mut projection = get_projection(&window);
    let mut first_person =
        FirstPerson::new([0.0, 1000.0, 0.0], FirstPersonSettings::keyboard_wasd());
    first_person.settings.speed_vertical = 5000.0;
    first_person.settings.speed_horizontal = 5000.0;

    let mut detached_camera = false;
    let mut camera_position = cgmath::Point3::new(0.0, 0.0, 0.0);

    let mut text = gfx_text::new(window.factory.clone())
        .with_size(12)
        .build()
        .unwrap();

    let mut fps_counter = FPSCounter::new();
    let mut last_frame = Instant::now();
    while let Some(e) = window.next() {
        first_person.event(&e);

        if let Some(_) = e.resize_args() {
            projection = get_projection(&window);
        }
        if let Some(Button::Keyboard(key)) = e.press_args() {
            if key == Key::Tab {
                detached_camera = !detached_camera;
            }
        }

        window.draw_3d(&e, |window| {
            let args = e.render_args().unwrap();

            let now = Instant::now();
            let dt = (now - last_frame).as_secs() as f32
                + (now - last_frame).subsec_nanos() as f32 / 1000_000_000.0;
            last_frame = now;

            window.encoder.clear_depth(&window.output_stencil, 1.0);
            window
                .encoder
                .clear(&window.output_color, [0.3, 0.3, 0.3, 1.0]);
            window.encoder.clear_depth(&window.output_stencil, 1.0);

            let mut camera = first_person.camera(args.ext_dt);
            if !detached_camera {
                let center_distance = (camera.position[0] * camera.position[0]
                    + camera.position[2] * camera.position[2])
                    .sqrt();

                if center_distance > 30000.0 {
                    first_person.position[0] = camera.position[0] / (center_distance / 30000.0);
                    first_person.position[2] = camera.position[2] / (center_distance / 30000.0);
                    camera = first_person.camera(0.0);
                }
                // if camera.position[1] > 19500.0 {
                //     first_person.position[1] = 19500.0;
                //     camera = first_person.camera(0.0);
                // }

                camera_position =
                    cgmath::Point3::new(camera.position[0], camera.position[1], camera.position[2]);
            }
            // if let Some(h) = terrain.get_height(cgmath::Point2::new(
            //     camera.position[0],
            //     camera.position[2],
            // ))
            // {
            //     camera.position[1] += h + 2.0;
            // }
            first_person.settings.speed_vertical =
                (5.0 * first_person.position[1] as f32).max(100.0f32);

            terrain.update(
                model_view_projection(vecmath::mat4_id(), camera.orthogonal(), projection),
                camera_position,
                &mut window.encoder,
                dt,
            );
            terrain.render(&mut window.encoder);
            terrain.render_sky(&mut window.encoder);

            let text_color = [0.0, 1.0, 1.0, 1.0];
            let fps = fps_counter.tick();
            text.add(&format!("FPS: {}", fps), [5, 5], text_color);
            text.add(
                &format!("Frame time: {:.1}", 1000.0 / fps as f32),
                [5, 17],
                text_color,
            );
            text.draw(&mut window.encoder, &window.output_color)
                .unwrap();
        });
    }
}
