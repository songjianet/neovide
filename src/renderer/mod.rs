use std::collections::HashMap;
use std::sync::Arc;

use log::trace;
use skulpin::skia_safe::gpu::SurfaceOrigin;
use skulpin::skia_safe::{
    colors, dash_path_effect, Budgeted, Canvas, ImageInfo, Paint, Rect, Surface
};
use skulpin::CoordinateSystemHelper;

mod caching_shaper;
pub mod cursor_renderer;
pub mod font_options;

pub use caching_shaper::CachingShaper;
pub use font_options::*;

use crate::editor::{Style, WindowRenderInfo, EDITOR};
use crate::redraw_scheduler::REDRAW_SCHEDULER;
use cursor_renderer::CursorRenderer;

pub struct RenderedWindow {
    surface: Surface,
    current_position: (f32, f32)
}

pub struct Renderer {
    rendered_windows: HashMap<u64, RenderedWindow>,
    paint: Paint,
    shaper: CachingShaper,

    pub font_width: f32,
    pub font_height: f32,
    pub window_regions: Vec<(u64, Rect)>,
    cursor_renderer: CursorRenderer,
}

impl Renderer {
    pub fn new() -> Renderer {
        let rendered_windows = HashMap::new();

        let mut paint = Paint::new(colors::WHITE, None);
        paint.set_anti_alias(false);

        let mut shaper = CachingShaper::new();

        let (font_width, font_height) = shaper.font_base_dimensions();
        let window_regions = Vec::new();
        let cursor_renderer = CursorRenderer::new();

        Renderer {
            rendered_windows,
            paint,
            shaper,
            font_width,
            font_height,
            window_regions,
            cursor_renderer,
        }
    }

    fn update_font(&mut self, guifont_setting: &str) -> bool {
        let updated = self.shaper.update_font(guifont_setting);
        if updated {
            let (font_width, font_height) = self.shaper.font_base_dimensions();
            self.font_width = font_width;
            self.font_height = font_height.ceil();
        }
        updated
    }

    fn compute_text_region(&self, grid_pos: (u64, u64), cell_width: u64) -> Rect {
        let (grid_x, grid_y) = grid_pos;
        let x = grid_x as f32 * self.font_width;
        let y = grid_y as f32 * self.font_height;
        let width = cell_width as f32 * self.font_width as f32;
        let height = self.font_height as f32;
        Rect::new(x, y, x + width, y + height)
    }

    fn draw_background(
        &mut self,
        canvas: &mut Canvas,
        grid_pos: (u64, u64),
        cell_width: u64,
        style: &Option<Arc<Style>>,
        default_style: &Arc<Style>,
    ) {
        let region = self.compute_text_region(grid_pos, cell_width);
        let style = style.as_ref().unwrap_or(default_style);

        self.paint
            .set_color(style.background(&default_style.colors).to_color());
        canvas.draw_rect(region, &self.paint);
    }

    fn draw_foreground(
        &mut self,
        canvas: &mut Canvas,
        text: &str,
        grid_pos: (u64, u64),
        cell_width: u64,
        style: &Option<Arc<Style>>,
        default_style: &Arc<Style>,
    ) {
        let (grid_x, grid_y) = grid_pos;
        let x = grid_x as f32 * self.font_width;
        let y = grid_y as f32 * self.font_height;
        let width = cell_width as f32 * self.font_width;

        let style = style.as_ref().unwrap_or(default_style);

        canvas.save();

        let region = self.compute_text_region(grid_pos, cell_width);

        canvas.clip_rect(region, None, Some(false));

        if style.underline || style.undercurl {
            let line_position = self.shaper.underline_position();
            let stroke_width = self.shaper.options.size / 10.0;
            self.paint
                .set_color(style.special(&default_style.colors).to_color());
            self.paint.set_stroke_width(stroke_width);

            if style.undercurl {
                self.paint.set_path_effect(dash_path_effect::new(
                    &[stroke_width * 2.0, stroke_width * 2.0],
                    0.0,
                ));
            } else {
                self.paint.set_path_effect(None);
            }

            canvas.draw_line(
                (x, y - line_position + self.font_height),
                (x + width, y - line_position + self.font_height),
                &self.paint,
            );
        }

        self.paint
            .set_color(style.foreground(&default_style.colors).to_color());
        let text = text.trim_end();
        if !text.is_empty() {
            for blob in self
                .shaper
                .shape_cached(text, style.bold, style.italic)
                .iter()
            {
                canvas.draw_text_blob(blob, (x, y), &self.paint);
            }
        }

        if style.strikethrough {
            let line_position = region.center_y();
            self.paint
                .set_color(style.special(&default_style.colors).to_color());
            canvas.draw_line((x, line_position), (x + width, line_position), &self.paint);
        }

        canvas.restore();
    }

    pub fn build_window_surface(
        &self,
        gpu_canvas: &mut Canvas,
        default_style: &Arc<Style>,
        dimensions: (i32, i32),
    ) -> Surface {
        let mut context = gpu_canvas.gpu_context().unwrap();
        let budgeted = Budgeted::Yes;
        let parent_image_info = gpu_canvas.image_info();
        let image_info = ImageInfo::new(
            dimensions,
            parent_image_info.color_type(),
            parent_image_info.alpha_type(),
            parent_image_info.color_space(),
        );
        let surface_origin = SurfaceOrigin::TopLeft;
        let mut surface = Surface::new_render_target(
            &mut context,
            budgeted,
            &image_info,
            None,
            surface_origin,
            None,
            None,
        )
        .expect("Could not create surface");
        let canvas = surface.canvas();
        canvas.clear(default_style.colors.background.clone().unwrap().to_color());
        surface
    }

    pub fn draw_window(
        &mut self,
        root_canvas: &mut Canvas,
        window_render_info: &WindowRenderInfo,
        default_style: &Arc<Style>,
    ) -> (u64, Rect) {
        let (grid_left, grid_top) = window_render_info.grid_position;
        let target_left = grid_left as f32 * self.font_width;
        let target_top = grid_top as f32 * self.font_height;

        let image_width = (window_render_info.width as f32 * self.font_width) as i32;
        let image_height = (window_render_info.height as f32 * self.font_height) as i32;

        let mut rendered_window = if window_render_info.should_clear {
            None
        } else {
            self.rendered_windows.remove(&window_render_info.grid_id)
        }
        .unwrap_or_else(|| {
            let surface = self.build_window_surface(root_canvas, &default_style, (image_width, image_height));
            RenderedWindow {
                surface,
                current_position: (target_left, target_top)
            }
        });

        if rendered_window.surface.width() != image_width || rendered_window.surface.height() != image_height {
            let mut old_surface = rendered_window.surface;
            rendered_window.surface = self.build_window_surface(root_canvas, &default_style, (image_width, image_height));
            old_surface.draw(rendered_window.surface.canvas(), (0.0, 0.0), None);
        }

        let (current_left, current_top) = rendered_window.current_position;
        let current_left = current_left + (target_left - current_left) * 0.4;
        let current_top = current_top +  (target_top - current_top) * 0.4;
        rendered_window.current_position = (current_left, current_top);

        let mut canvas = rendered_window.surface.canvas();

        for command in window_render_info.draw_commands.iter() {
            self.draw_background(
                &mut canvas,
                command.grid_position,
                command.cell_width,
                &command.style,
                &default_style,
            );
        }

        for command in window_render_info.draw_commands.iter() {
            self.draw_foreground(
                &mut canvas,
                &command.text,
                command.grid_position,
                command.cell_width,
                &command.style,
                &default_style,
            );
        }

        root_canvas.save_layer(&Default::default());

        unsafe {
            rendered_window.surface.draw(root_canvas.surface().unwrap().canvas(), (current_left, current_top), None);
        }

        root_canvas.restore();

        self.rendered_windows.insert(window_render_info.grid_id, rendered_window);

        (window_render_info.grid_id, Rect::new(current_left, current_top, current_left + image_width as f32, current_top + image_height as f32))
    }

    pub fn draw(
        &mut self,
        gpu_canvas: &mut Canvas,
        coordinate_system_helper: &CoordinateSystemHelper,
        dt: f32,
    ) -> bool {
        trace!("Rendering");

        REDRAW_SCHEDULER.queue_next_frame();

        let (render_info, default_style, cursor, guifont_setting) = {
            let mut editor = EDITOR.lock();
            (
                editor.build_render_info(),
                editor.default_style.clone(),
                editor.cursor.clone(),
                editor.guifont.clone(),
            )
        };

        gpu_canvas.clear(default_style.colors.background.clone().unwrap().to_color());

        let font_changed = guifont_setting
            .map(|guifont| self.update_font(&guifont))
            .unwrap_or(false);

        for closed_window_id in render_info.closed_window_ids.iter() {
            self.rendered_windows.remove(&closed_window_id);
        }

        coordinate_system_helper.use_logical_coordinates(gpu_canvas);

        self.window_regions = render_info.windows
            .iter()
            .map(|window_render_info| self.draw_window(gpu_canvas, window_render_info, &default_style))
            .collect();

        self.cursor_renderer.draw(
            cursor,
            &default_style.colors,
            (self.font_width, self.font_height),
            &mut self.shaper,
            gpu_canvas,
            dt,
        );

        font_changed
    }
}
