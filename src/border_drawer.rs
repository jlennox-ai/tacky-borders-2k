use anyhow::Context;
use std::time;
use windows::Win32::Foundation::{HWND, POINT};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D_RECT_F, D2D_SIZE_F, D2D1_COLOR_F, D2D1_COMPOSITE_MODE_SOURCE_OVER,
    D2D1_FIGURE_BEGIN_HOLLOW, D2D1_FIGURE_END_OPEN,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1_ARC_SEGMENT, D2D1_ARC_SIZE_SMALL, D2D1_BRUSH_PROPERTIES,
    D2D1_INTERPOLATION_MODE_LINEAR, D2D1_ROUNDED_RECT, D2D1_SWEEP_DIRECTION_CLOCKWISE, ID2D1Brush,
    ID2D1GeometrySink, ID2D1Multithread, ID2D1PathGeometry1, ID2D1RenderTarget,
};
use windows::Win32::Graphics::Dxgi::IDXGISurface;
use windows::core::Interface;
use windows_numerics::{Matrix3x2, Vector2};

use crate::APP_STATE;
use crate::animations::{AnimType, Animations};
use crate::border_config::BorderConfig;
use crate::colors::ColorBrush;
use crate::config::BorderStyleConfig;
use crate::effects::Effects;
use crate::render_backend::{RenderBackend, RenderBackendConfig, TARGET_BITMAP_PROPS};
use crate::utils::{
    StandaloneWindowsError, T_E_UNINIT, ToWindowsResult, WindowsCompatibleError,
    WindowsCompatibleResult, WindowsContext, WriteLockable,
};
use crate::window_border::WindowState;

#[derive(Debug, Default)]
pub struct BorderDrawer {
    pub stroke_width: i32,
    // This is WriteLockable so it doesn't accidentally change when the tracking window is in
    // the snapped/arranged state where the borders are supposed to remain square
    pub corner_radius: WriteLockable<f32>,
    pub render_backend: RenderBackend,
    pub style: BorderStyleConfig,
    pub active_color: ColorBrush,
    pub inactive_color: ColorBrush,
    pub animations: Animations,
    pub effects: Effects,
    pub last_render_time: Option<time::Instant>,
    pub last_anim_time: Option<time::Instant>,
}

impl BorderDrawer {
    pub fn configure_appearance(&mut self, config: &BorderConfig, dpi: u32, tracking_window: HWND) {
        let stroke_width = config.width_at(dpi);
        let corner_radius = config.radius_at(stroke_width, dpi, tracking_window);

        self.stroke_width = stroke_width;
        self.corner_radius = WriteLockable::new(corner_radius);
        self.style = config.style;
        self.active_color = config.active_color.to_color_brush(true);
        self.inactive_color = config.inactive_color.to_color_brush(false);
        self.animations = config.animations.to_animations();
        self.effects = config.effects.to_effects(dpi);
    }

    pub fn init(
        &mut self,
        width: u32,
        height: u32,
        border_window: HWND,
        bounds: D2D_RECT_F,
        render_backend_config: RenderBackendConfig,
    ) -> WindowsCompatibleResult<()> {
        // Drop our current render backend to avoid issues with recreating existing resources when
        // calling init() multiple times in a row
        self.render_backend = RenderBackend::None;

        self.render_backend = render_backend_config
            .to_render_backend(width, height, border_window, self.effects.is_enabled())
            .windows_context("could not initialize render backend in init()")?;

        let renderer: &ID2D1RenderTarget = match self.render_backend {
            RenderBackend::V2(ref backend) => &backend.d2d_context,
            RenderBackend::Legacy(ref backend) => &backend.render_target,
            RenderBackend::None => {
                return Err(WindowsCompatibleError::Standalone(
                    StandaloneWindowsError::new(T_E_UNINIT, "render backend is None"),
                ));
            }
        };

        // We will adjust opacity later. For now, we set it to 0.
        let brush_properties = D2D1_BRUSH_PROPERTIES {
            opacity: 0.0,
            transform: Matrix3x2::identity(),
        };
        self.active_color
            .init_brush(renderer, &bounds, &brush_properties)?;
        self.inactive_color
            .init_brush(renderer, &bounds, &brush_properties)?;

        if self.render_backend.supports_effects() {
            self.effects
                .init_command_lists_if_enabled(&self.render_backend)
                .windows_context("could not initialize command list")?;
        }

        Ok(())
    }

    pub fn uninit(&mut self) {
        self.render_backend = RenderBackend::None;
        let _ = self.active_color.take_brush();
        let _ = self.inactive_color.take_brush();
        let _ = self.effects.take_active_command_list();
        let _ = self.effects.take_inactive_command_list();
    }

    pub fn resize_renderer(&mut self, width: u32, height: u32) -> WindowsCompatibleResult<()> {
        self.render_backend
            .resize(width, height, self.effects.is_enabled())
            .windows_context("could not update render resources")?;

        if self.render_backend.supports_effects() {
            self.effects
                .init_command_lists_if_enabled(&self.render_backend)
                .context("could not initialize command lists")
                .to_windows_result(T_E_UNINIT)?;
        }

        Ok(())
    }

    /// Renders a border onto the internal bitmap along the inside edge of the bounds. Effects
    /// (e.g. glow) are drawn outside, so the caller should pad the bounds if needed to prevent clipping.
    ///
    /// NOTE: bound coordinates should be specified relative to the bitmap, not the screen.
    pub fn render(
        &mut self,
        bounds: D2D_RECT_F,
        window_state: WindowState,
    ) -> WindowsCompatibleResult<()> {
        self.last_render_time = Some(time::Instant::now());

        // Direct2D draws a stroke centered along a given rect, but we want it to be drawn on the
        // inside of 'bounds'. To achieve this, we pad it by half the stroke width.
        let half_stroke_width = self.stroke_width as f32 / 2.0;
        let stroke_rect = D2D1_ROUNDED_RECT {
            rect: D2D_RECT_F {
                left: bounds.left + half_stroke_width,
                top: bounds.top + half_stroke_width,
                right: bounds.right - half_stroke_width,
                bottom: bounds.bottom - half_stroke_width,
            },
            radiusX: *self.corner_radius.get(),
            radiusY: *self.corner_radius.get(),
        };

        // Note that Rust's borrow checker prevents passing the render backend from the match arm,
        // so I'll need to grab it from within the respective functions instead
        match self.render_backend {
            RenderBackend::V2(_) if self.effects.should_apply(window_state) => {
                self.render_v2_with_effects(stroke_rect, bounds, window_state)?
            }
            RenderBackend::V2(_) => self.render_v2(stroke_rect, bounds, window_state)?,
            RenderBackend::Legacy(_) => self.render_legacy(stroke_rect, bounds, window_state)?,
            RenderBackend::None => {
                return Err(WindowsCompatibleError::Standalone(
                    StandaloneWindowsError::new(T_E_UNINIT, "render_backend is None"),
                ));
            }
        }

        Ok(())
    }

    fn render_legacy(
        &mut self,
        stroke_rect: D2D1_ROUNDED_RECT,
        bounds: D2D_RECT_F,
        window_state: WindowState,
    ) -> WindowsCompatibleResult<()> {
        let RenderBackend::Legacy(ref backend) = self.render_backend else {
            return Err(WindowsCompatibleError::Standalone(
                StandaloneWindowsError::new(
                    T_E_UNINIT,
                    "could not get render_backend within render()",
                ),
            ));
        };
        let render_target = &backend.render_target;

        unsafe {
            // Determine which color should be drawn on top (for color fade animation)
            let (bottom_color, top_color) = match window_state {
                WindowState::Active => (&self.inactive_color, &self.active_color),
                WindowState::Inactive => (&self.active_color, &self.inactive_color),
            };

            render_target.BeginDraw();
            render_target.Clear(None);

            if bottom_color.get_opacity().to_windows_result(T_E_UNINIT)? > 0.0 {
                if let ColorBrush::Gradient(gradient) = bottom_color {
                    gradient.update_start_end_points(&bounds);
                }

                match bottom_color.get_brush() {
                    Some(id2d1_brush) => {
                        self.draw_border(&stroke_rect, render_target, bottom_color, id2d1_brush)?
                    }
                    None => debug!("ID2D1Brush for bottom_color has not been created yet"),
                }
            }
            if top_color.get_opacity().to_windows_result(T_E_UNINIT)? > 0.0 {
                if let ColorBrush::Gradient(gradient) = top_color {
                    gradient.update_start_end_points(&bounds);
                }

                match top_color.get_brush() {
                    Some(id2d1_brush) => {
                        self.draw_border(&stroke_rect, render_target, top_color, id2d1_brush)?
                    }
                    None => debug!("ID2D1Brush for top_color has not been created yet"),
                }
            }

            render_target.EndDraw(None, None)?;
        }

        Ok(())
    }

    fn render_v2(
        &mut self,
        stroke_rect: D2D1_ROUNDED_RECT,
        bounds: D2D_RECT_F,
        window_state: WindowState,
    ) -> WindowsCompatibleResult<()> {
        let RenderBackend::V2(ref backend) = self.render_backend else {
            return Err(WindowsCompatibleError::Standalone(
                StandaloneWindowsError::new(
                    T_E_UNINIT,
                    "could not get render_backend within render()",
                ),
            ));
        };
        let d2d_context = &backend.d2d_context;

        unsafe {
            // Determine which color should be drawn on top (for color fade animation)
            let (bottom_color, top_color) = match window_state {
                WindowState::Active => (&self.inactive_color, &self.active_color),
                WindowState::Inactive => (&self.active_color, &self.inactive_color),
            };

            // We're about to use DirectComposition which means we will be using the underlying
            // Direct3D objects without Direct2D's knowledge. To avoid resource access conflict, we
            // must explicitly acquire a lock. Read the following article for more info:
            // https://learn.microsoft.com/en-us/windows/win32/direct2d/multi-threaded-direct2d-apps
            let d2d_multithread: ID2D1Multithread = APP_STATE
                .render_factory
                .cast()
                .windows_context("d2d_multithread")?;
            d2d_multithread.Enter();

            // Set d2d_context's target back to the target_bitmap so we can draw to the display
            let mut point = POINT::default();
            let dxgi_surface: IDXGISurface = backend
                .d_comp_surface
                .BeginDraw(None, &mut point)
                .windows_context("dxgi_surface")?;
            let target_bitmap = d2d_context
                .CreateBitmapFromDxgiSurface(&dxgi_surface, Some(&TARGET_BITMAP_PROPS))
                .windows_context("target_bitmap")?;
            d2d_context.SetTarget(&target_bitmap);

            // Draw to the target_bitmap
            d2d_context.BeginDraw();
            d2d_context.Clear(None);

            if bottom_color.get_opacity().to_windows_result(T_E_UNINIT)? > 0.0 {
                if let ColorBrush::Gradient(gradient) = bottom_color {
                    gradient.update_start_end_points(&bounds);
                }

                match bottom_color.get_brush() {
                    Some(id2d1_brush) => {
                        self.draw_border(&stroke_rect, d2d_context, bottom_color, id2d1_brush)?
                    }
                    None => debug!("ID2D1Brush for bottom_color has not been created yet"),
                }
            }
            if top_color.get_opacity().to_windows_result(T_E_UNINIT)? > 0.0 {
                if let ColorBrush::Gradient(gradient) = top_color {
                    gradient.update_start_end_points(&bounds);
                }

                match top_color.get_brush() {
                    Some(id2d1_brush) => {
                        self.draw_border(&stroke_rect, d2d_context, top_color, id2d1_brush)?
                    }
                    None => debug!("ID2D1Brush for top_color has not been created yet"),
                }
            }

            d2d_context.EndDraw(None, None)?;

            d2d_context.SetTarget(None);
            backend
                .d_comp_surface
                .EndDraw()
                .windows_context("d_comp_surface.EndDraw()")?;
            backend
                .d_comp_device
                .Commit()
                .windows_context("d_comp_device.Commit()")?;

            d2d_multithread.Leave();
        }

        Ok(())
    }

    fn render_v2_with_effects(
        &mut self,
        stroke_rect: D2D1_ROUNDED_RECT,
        bounds: D2D_RECT_F,
        window_state: WindowState,
    ) -> WindowsCompatibleResult<()> {
        let RenderBackend::V2(ref backend) = self.render_backend else {
            return Err(WindowsCompatibleError::Standalone(
                StandaloneWindowsError::new(
                    T_E_UNINIT,
                    "could not get render_backend within render()",
                ),
            ));
        };
        let d2d_context = &backend.d2d_context;

        let half_stroke_width = stroke_rect.rect.left - bounds.left;

        unsafe {
            // Determine which color should be drawn on top (for color fade animation)
            let (bottom_color, top_color) = match window_state {
                WindowState::Active => (&self.inactive_color, &self.active_color),
                WindowState::Inactive => (&self.active_color, &self.inactive_color),
            };

            // Create a rect that covers up to the outer edge of the border
            let border_outer_rect = D2D1_ROUNDED_RECT {
                rect: bounds,
                radiusX: stroke_rect.radiusX + half_stroke_width,
                radiusY: stroke_rect.radiusY + half_stroke_width,
            };

            // Set the d2d_context target to the border_bitmap
            let border_bitmap = backend
                .border_bitmap
                .as_ref()
                .context("could not get border_bitmap")
                .to_windows_result(T_E_UNINIT)?;
            d2d_context.SetTarget(border_bitmap);

            // Draw to the border_bitmap
            d2d_context.BeginDraw();
            d2d_context.Clear(None);

            // We use filled rectangles here because it helps make the effects more visible.
            // Additionally, if someone sets the stroke width to 0, the effects will still be
            // visible (whereas they wouldn't be if we used a hollow rectangle).
            if bottom_color.get_opacity().to_windows_result(T_E_UNINIT)? > 0.0 {
                if let ColorBrush::Gradient(gradient) = bottom_color {
                    gradient.update_start_end_points(&bounds);
                }

                match bottom_color.get_brush() {
                    Some(id2d1_brush) => {
                        self.fill_rectangle(&border_outer_rect, d2d_context, id2d1_brush)
                    }
                    None => debug!("ID2D1Brush for bottom_color has not been created yet"),
                }
            }
            if top_color.get_opacity().to_windows_result(T_E_UNINIT)? > 0.0 {
                if let ColorBrush::Gradient(gradient) = top_color {
                    gradient.update_start_end_points(&bounds);
                }

                match top_color.get_brush() {
                    Some(id2d1_brush) => {
                        self.fill_rectangle(&border_outer_rect, d2d_context, id2d1_brush)
                    }
                    None => debug!("ID2D1Brush for top_color has not been created yet"),
                }
            }

            d2d_context.EndDraw(None, None)?;
        }

        unsafe {
            // Set the d2d_context target to the mask_bitmap to create an alpha mask
            let mask_bitmap = backend
                .mask_bitmap
                .as_ref()
                .context("could not get mask_bitmap")
                .to_windows_result(T_E_UNINIT)?;
            d2d_context.SetTarget(mask_bitmap);

            // Create a rect that covers up to the inner edge of the border
            // This rect is used to mask out the inner portion of the border
            let border_inner_rect = D2D1_ROUNDED_RECT {
                rect: D2D_RECT_F {
                    left: stroke_rect.rect.left + half_stroke_width,
                    top: stroke_rect.rect.top + half_stroke_width,
                    right: stroke_rect.rect.right - half_stroke_width,
                    bottom: stroke_rect.rect.bottom - half_stroke_width,
                },
                radiusX: stroke_rect.radiusX - half_stroke_width,
                radiusY: stroke_rect.radiusY - half_stroke_width,
            };

            // Create a 100% opaque brush because our active/inactive colors' brushes might not be
            let opaque_brush = d2d_context.CreateSolidColorBrush(
                &D2D1_COLOR_F {
                    r: 1.0,
                    g: 1.0,
                    b: 1.0,
                    a: 1.0,
                },
                None,
            )?;

            d2d_context.BeginDraw();
            d2d_context.Clear(None);

            self.fill_rectangle(&border_inner_rect, d2d_context, &opaque_brush);

            d2d_context.EndDraw(None, None)?;
        }

        unsafe {
            // We're about to use DirectComposition which means we will be using the underlying
            // Direct3D objects without Direct2D's knowledge. To avoid resource access conflict, we
            // must explicitly acquire a lock. Read the following article for more info:
            // https://learn.microsoft.com/en-us/windows/win32/direct2d/multi-threaded-direct2d-apps
            let d2d_multithread: ID2D1Multithread = APP_STATE
                .render_factory
                .cast()
                .windows_context("d2d_multithread")?;
            d2d_multithread.Enter();

            // Set d2d_context's target back to the target_bitmap so we can draw to the display
            let mut point = POINT::default();
            let dxgi_surface: IDXGISurface = backend
                .d_comp_surface
                .BeginDraw(None, &mut point)
                .windows_context("dxgi_surface")?;
            let target_bitmap = d2d_context
                .CreateBitmapFromDxgiSurface(&dxgi_surface, Some(&TARGET_BITMAP_PROPS))
                .windows_context("target_bitmap")?;
            d2d_context.SetTarget(&target_bitmap);

            // Retrieve our command list (includes border_bitmap, mask_bitmap, and effects)
            let command_list = self
                .effects
                .get_current_command_list(window_state)
                .to_windows_result(T_E_UNINIT)?;

            // Draw to the target_bitmap
            d2d_context.BeginDraw();
            d2d_context.Clear(None);

            d2d_context.DrawImage(
                command_list,
                None,
                None,
                D2D1_INTERPOLATION_MODE_LINEAR,
                D2D1_COMPOSITE_MODE_SOURCE_OVER,
            );

            d2d_context.EndDraw(None, None)?;

            d2d_context.SetTarget(None);
            backend
                .d_comp_surface
                .EndDraw()
                .windows_context("d_comp_surface.EndDraw()")?;
            backend
                .d_comp_device
                .Commit()
                .windows_context("d_comp_device.Commit()")?;

            d2d_multithread.Leave();
        }

        Ok(())
    }

    fn draw_border(
        &self,
        stroke_rect: &D2D1_ROUNDED_RECT,
        renderer: &ID2D1RenderTarget,
        color: &ColorBrush,
        brush: &ID2D1Brush,
    ) -> WindowsCompatibleResult<()> {
        match self.style {
            BorderStyleConfig::Modern => {
                self.draw_rectangle(stroke_rect, renderer, brush);
                Ok(())
            }
            BorderStyleConfig::Win2k => self.draw_win2k_frame(stroke_rect, renderer, color),
        }
    }

    /// Draws a classic Windows 2000-style frame: a dark outer outline, a raised bevel (light
    /// top-left, shadow bottom-right), a face band in the configured color, a sunken inner bevel,
    /// a dark inner outline, and corner separator notches like classic resize frames had.
    fn draw_win2k_frame(
        &self,
        stroke_rect: &D2D1_ROUNDED_RECT,
        renderer: &ID2D1RenderTarget,
        color: &ColorBrush,
    ) -> WindowsCompatibleResult<()> {
        let frame_width = self.stroke_width as f32;
        if frame_width < 1.0 {
            return Ok(());
        }

        // stroke_rect is inset by half the stroke width; recover the border's outer edge
        let half_width = frame_width / 2.0;
        let outer = D2D_RECT_F {
            left: stroke_rect.rect.left - half_width,
            top: stroke_rect.rect.top - half_width,
            right: stroke_rect.rect.right + half_width,
            bottom: stroke_rect.rect.bottom + half_width,
        };
        let outer_radius = match stroke_rect.radiusX {
            0.0 => 0.0,
            radius => radius + half_width,
        };

        let face = color.base_color();
        let light = blend_toward(&face, 1.0, 0.75);
        let shadow = blend_toward(&face, 0.0, 0.45);
        let dark = blend_toward(&face, 0.0, 0.78);

        // These brushes are recreated every render, but D2D solid color brushes are cheap
        let brush_properties = D2D1_BRUSH_PROPERTIES {
            opacity: color.get_opacity().to_windows_result(T_E_UNINIT)?,
            transform: Matrix3x2::identity(),
        };
        let (light_brush, shadow_brush, dark_brush, face_brush) = unsafe {
            (
                renderer.CreateSolidColorBrush(&light, Some(&brush_properties))?,
                renderer.CreateSolidColorBrush(&shadow, Some(&brush_properties))?,
                renderer.CreateSolidColorBrush(&dark, Some(&brush_properties))?,
                renderer.CreateSolidColorBrush(&face, Some(&brush_properties))?,
            )
        };
        let light_brush: &ID2D1Brush = (&light_brush).into();
        let shadow_brush: &ID2D1Brush = (&shadow_brush).into();
        let dark_brush: &ID2D1Brush = (&dark_brush).into();
        let face_brush: &ID2D1Brush = (&face_brush).into();

        // Band layout from the outside in: dark outline, raised bevel, face, sunken bevel, dark
        // outline. 'unit' is the 1px-equivalent line thickness, scaled with the frame width.
        let unit = (frame_width / 6.0).max(1.0);
        let face_width = (frame_width - unit * 4.0).max(0.0);
        let mut inset = 0.0f32;

        self.stroke_ring(renderer, &outer, outer_radius, inset, unit, dark_brush);
        inset += unit;

        self.stroke_bevel_ring(
            renderer,
            &outer,
            outer_radius,
            inset,
            unit,
            light_brush,
            shadow_brush,
        )?;
        inset += unit;

        if face_width > 0.0 {
            self.stroke_ring(renderer, &outer, outer_radius, inset, face_width, face_brush);
            inset += face_width;
        }

        self.stroke_bevel_ring(
            renderer,
            &outer,
            outer_radius,
            inset,
            unit,
            shadow_brush,
            light_brush,
        )?;
        inset += unit;

        self.stroke_ring(renderer, &outer, outer_radius, inset, unit, dark_brush);

        self.draw_corner_separators(renderer, &outer, frame_width, unit, dark_brush);

        Ok(())
    }

    /// Strokes one concentric ring of the frame, 'inset' pixels in from the outer edge.
    fn stroke_ring(
        &self,
        renderer: &ID2D1RenderTarget,
        outer: &D2D_RECT_F,
        outer_radius: f32,
        inset: f32,
        thickness: f32,
        brush: &ID2D1Brush,
    ) {
        let center_inset = inset + thickness / 2.0;
        let rect = D2D_RECT_F {
            left: outer.left + center_inset,
            top: outer.top + center_inset,
            right: outer.right - center_inset,
            bottom: outer.bottom - center_inset,
        };
        let radius = (outer_radius - center_inset).max(0.0);

        unsafe {
            if radius <= 0.0 {
                renderer.DrawRectangle(&rect, brush, thickness, None);
            } else {
                let rounded_rect = D2D1_ROUNDED_RECT {
                    rect,
                    radiusX: radius,
                    radiusY: radius,
                };
                renderer.DrawRoundedRectangle(&rounded_rect, brush, thickness, None);
            }
        }
    }

    /// Strokes one ring in two halves for a 3D bevel: the top-left half (split at the corner arc
    /// midpoints of the top-right and bottom-left corners) in one color, the rest in another.
    fn stroke_bevel_ring(
        &self,
        renderer: &ID2D1RenderTarget,
        outer: &D2D_RECT_F,
        outer_radius: f32,
        inset: f32,
        thickness: f32,
        top_left_brush: &ID2D1Brush,
        bottom_right_brush: &ID2D1Brush,
    ) -> WindowsCompatibleResult<()> {
        let center_inset = inset + thickness / 2.0;
        let rect = D2D_RECT_F {
            left: outer.left + center_inset,
            top: outer.top + center_inset,
            right: outer.right - center_inset,
            bottom: outer.bottom - center_inset,
        };
        let radius = (outer_radius - center_inset).max(0.0);

        let top_left_path = build_half_ring_path(&rect, radius, true)?;
        let bottom_right_path = build_half_ring_path(&rect, radius, false)?;

        unsafe {
            renderer.DrawGeometry(&top_left_path, top_left_brush, thickness, None);
            renderer.DrawGeometry(&bottom_right_path, bottom_right_brush, thickness, None);
        }

        Ok(())
    }

    /// Draws the small notches that separate the corner sizing regions from the edges, like
    /// classic resize frames had.
    fn draw_corner_separators(
        &self,
        renderer: &ID2D1RenderTarget,
        outer: &D2D_RECT_F,
        frame_width: f32,
        unit: f32,
        brush: &ID2D1Brush,
    ) {
        let offset = frame_width * 5.0;

        // Skip the separators if the window is too small for distinct corner regions
        let window_width = outer.right - outer.left;
        let window_height = outer.bottom - outer.top;
        if window_width <= (offset + unit) * 2.0 || window_height <= (offset + unit) * 2.0 {
            return;
        }

        let separator_rects = [
            // Top edge
            (outer.left + offset, outer.top, unit, frame_width),
            (outer.right - offset - unit, outer.top, unit, frame_width),
            // Bottom edge
            (outer.left + offset, outer.bottom - frame_width, unit, frame_width),
            (outer.right - offset - unit, outer.bottom - frame_width, unit, frame_width),
            // Left edge
            (outer.left, outer.top + offset, frame_width, unit),
            (outer.left, outer.bottom - offset - unit, frame_width, unit),
            // Right edge
            (outer.right - frame_width, outer.top + offset, frame_width, unit),
            (outer.right - frame_width, outer.bottom - offset - unit, frame_width, unit),
        ];

        unsafe {
            for (left, top, width, height) in separator_rects {
                let rect = D2D_RECT_F {
                    left,
                    top,
                    right: left + width,
                    bottom: top + height,
                };
                renderer.FillRectangle(&rect, brush);
            }
        }
    }

    // NOTE: ID2D1DeviceContext implements From<&ID2D1DeviceContext> for &ID2D1RenderTarget
    fn draw_rectangle(
        &self,
        stroke_rect: &D2D1_ROUNDED_RECT,
        renderer: &ID2D1RenderTarget,
        brush: &ID2D1Brush,
    ) {
        unsafe {
            match stroke_rect.radiusX {
                0.0 => {
                    renderer.DrawRectangle(&stroke_rect.rect, brush, self.stroke_width as f32, None)
                }
                _ => renderer.DrawRoundedRectangle(
                    stroke_rect,
                    brush,
                    self.stroke_width as f32,
                    None,
                ),
            }
        }
    }

    // NOTE: ID2D1DeviceContext implements From<&ID2D1DeviceContext> for &ID2D1RenderTarget
    fn fill_rectangle(
        &self,
        rounded_rect: &D2D1_ROUNDED_RECT,
        renderer: &ID2D1RenderTarget,
        brush: &ID2D1Brush,
    ) {
        unsafe {
            match rounded_rect.radiusX {
                0.0 => renderer.FillRectangle(&rounded_rect.rect, brush),
                _ => renderer.FillRoundedRectangle(rounded_rect, brush),
            }
        }
    }

    pub fn set_anims_timer_if_needed(&mut self, border_window: HWND) {
        self.animations
            .set_timer_if_needed(border_window, &mut self.last_anim_time);
    }

    pub fn destroy_anims_timer(&mut self) {
        self.animations.destroy_timer();
    }

    pub fn animate(&mut self, bounds: D2D_RECT_F, window_state: WindowState) -> anyhow::Result<()> {
        let anim_elapsed = self
            .last_anim_time
            .get_or_insert_with(time::Instant::now)
            .elapsed();
        let render_elapsed = self
            .last_render_time
            .get_or_insert_with(time::Instant::now)
            .elapsed();

        let mut update = false;

        for anim_params in self.animations.get_current(window_state).clone().iter() {
            match anim_params.anim_type {
                AnimType::Spiral | AnimType::ReverseSpiral => {
                    self.animations.animate_spiral(
                        &bounds,
                        &self.active_color,
                        &self.inactive_color,
                        &anim_elapsed,
                        anim_params,
                    );
                    update = true;
                }
                AnimType::Fade => {
                    let correct_active_opacity = if window_state == WindowState::Active {
                        1.0
                    } else {
                        0.0
                    };

                    if self.active_color.get_opacity()? != correct_active_opacity
                        || self.inactive_color.get_opacity()? != 1.0 - correct_active_opacity
                    {
                        self.animations.animate_fade(
                            window_state,
                            &self.active_color,
                            &self.inactive_color,
                            &anim_elapsed,
                            anim_params,
                        )?;
                        update = true;
                    }
                }
            }
        }

        self.last_anim_time = Some(time::Instant::now());

        let render_interval = 1.0 / self.animations.fps as f32;
        let time_diff = render_elapsed.as_secs_f32() - render_interval;
        if update && (time_diff.abs() <= 0.001 || time_diff >= 0.0) {
            self.render(bounds, window_state)?;
        }

        Ok(())
    }
}

/// Linearly blends each color channel toward 'target' (0.0 = black, 1.0 = white) by 'amount',
/// preserving alpha. Used to derive the Win2k bevel tones from the face color.
fn blend_toward(color: &D2D1_COLOR_F, target: f32, amount: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: color.r + (target - color.r) * amount,
        g: color.g + (target - color.g) * amount,
        b: color.b + (target - color.b) * amount,
        a: color.a,
    }
}

/// Builds an open path tracing half of a (rounded) rectangle's outline, splitting at the midpoint
/// of the top-right and bottom-left corner arcs (or at the corners themselves when radius is 0).
/// The top-left half runs from the bottom-left split point up and across to the top-right split
/// point; the bottom-right half is the remainder. Both are traced clockwise.
fn build_half_ring_path(
    rect: &D2D_RECT_F,
    radius: f32,
    top_left: bool,
) -> WindowsCompatibleResult<ID2D1PathGeometry1> {
    let geometry = unsafe { APP_STATE.render_factory.CreatePathGeometry()? };
    let sink = unsafe { geometry.Open()? };

    let r = radius;
    // Distance from a corner of 'rect' to its arc's 45-degree midpoint, along each axis
    let k = r * (1.0 - std::f32::consts::FRAC_1_SQRT_2);

    let point = |x: f32, y: f32| Vector2 { X: x, Y: y };
    let arc_to = |sink: &ID2D1GeometrySink, x: f32, y: f32| unsafe {
        sink.AddArc(&D2D1_ARC_SEGMENT {
            point: point(x, y),
            size: D2D_SIZE_F {
                width: r,
                height: r,
            },
            rotationAngle: 0.0,
            sweepDirection: D2D1_SWEEP_DIRECTION_CLOCKWISE,
            arcSize: D2D1_ARC_SIZE_SMALL,
        })
    };

    unsafe {
        if top_left {
            // Bottom-left arc midpoint -> up the left edge -> across the top edge -> top-right
            // arc midpoint
            sink.BeginFigure(
                point(rect.left + k, rect.bottom - k),
                D2D1_FIGURE_BEGIN_HOLLOW,
            );
            if r > 0.0 {
                arc_to(&sink, rect.left, rect.bottom - r);
            }
            sink.AddLine(point(rect.left, rect.top + r));
            if r > 0.0 {
                arc_to(&sink, rect.left + r, rect.top);
            }
            sink.AddLine(point(rect.right - r, rect.top));
            if r > 0.0 {
                arc_to(&sink, rect.right - k, rect.top + k);
            }
        } else {
            // Top-right arc midpoint -> down the right edge -> across the bottom edge ->
            // bottom-left arc midpoint
            sink.BeginFigure(point(rect.right - k, rect.top + k), D2D1_FIGURE_BEGIN_HOLLOW);
            if r > 0.0 {
                arc_to(&sink, rect.right, rect.top + r);
            }
            sink.AddLine(point(rect.right, rect.bottom - r));
            if r > 0.0 {
                arc_to(&sink, rect.right - r, rect.bottom);
            }
            sink.AddLine(point(rect.left + r, rect.bottom));
            if r > 0.0 {
                arc_to(&sink, rect.left + k, rect.bottom - k);
            }
        }
        sink.EndFigure(D2D1_FIGURE_END_OPEN);
        sink.Close()?;
    }

    Ok(geometry)
}
