use anyhow::Context;
use serde::Deserialize;
use std::mem::ManuallyDrop;
use windows::Win32::Foundation::{HWND, LUID, POINT};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D_RECT_F, D2D_SIZE_U, D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_PIXEL_FORMAT,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1_ANTIALIAS_MODE_ALIASED, D2D1_ANTIALIAS_MODE_PER_PRIMITIVE, D2D1_BITMAP_OPTIONS,
    D2D1_BITMAP_OPTIONS_CANNOT_DRAW, D2D1_BITMAP_OPTIONS_TARGET, D2D1_BITMAP_PROPERTIES1,
    D2D1_DEVICE_CONTEXT_OPTIONS_NONE, D2D1_HWND_RENDER_TARGET_PROPERTIES,
    D2D1_PRESENT_OPTIONS_IMMEDIATELY, D2D1_PRESENT_OPTIONS_RETAIN_CONTENTS,
    D2D1_RENDER_TARGET_PROPERTIES, D2D1_RENDER_TARGET_TYPE_DEFAULT, ID2D1Bitmap1,
    ID2D1DeviceContext, ID2D1HwndRenderTarget, ID2D1Multithread,
};
use windows::Win32::Graphics::DirectComposition::{
    DCompositionCreateDevice3, IDCompositionDesktopDevice, IDCompositionDevice3,
    IDCompositionSurface, IDCompositionTarget, IDCompositionVisual2,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_PREMULTIPLIED, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_UNKNOWN,
};
use windows::Win32::Graphics::Dxgi::IDXGISurface;
use windows::core::Interface;
use windows_numerics::Matrix3x2;

use crate::APP_STATE;
use crate::utils::{
    LogIfErr, StandaloneWindowsError, T_E_UNINIT, ToWindowsResult, WindowsCompatibleError,
    WindowsCompatibleResult, WindowsContext,
};

pub const TARGET_BITMAP_PROPS: D2D1_BITMAP_PROPERTIES1 = D2D1_BITMAP_PROPERTIES1 {
    bitmapOptions: D2D1_BITMAP_OPTIONS(
        D2D1_BITMAP_OPTIONS_TARGET.0 | D2D1_BITMAP_OPTIONS_CANNOT_DRAW.0,
    ),
    pixelFormat: D2D1_PIXEL_FORMAT {
        format: DXGI_FORMAT_B8G8R8A8_UNORM,
        alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
    },
    // TODO: Test scaling using these dpiX dpiY instead of manually scaling stroke width myself.
    // If I remember correctly, using these could lead to blurry edges, but I want to try again.
    dpiX: 96.0,
    dpiY: 96.0,
    colorContext: ManuallyDrop::new(None),
};
pub const EXTRA_BITMAP_PROPS: D2D1_BITMAP_PROPERTIES1 = D2D1_BITMAP_PROPERTIES1 {
    bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET,
    pixelFormat: D2D1_PIXEL_FORMAT {
        format: DXGI_FORMAT_B8G8R8A8_UNORM,
        alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
    },
    dpiX: 96.0,
    dpiY: 96.0,
    colorContext: ManuallyDrop::new(None),
};

#[derive(Debug, Default, Clone, Copy, Deserialize, PartialEq)]
pub enum RenderBackendConfig {
    #[default]
    V2,
    Legacy,
}

#[derive(Debug, Default, Clone)]
pub enum RenderBackend {
    V2(V2RenderBackend),
    Legacy(LegacyRenderBackend),
    #[default]
    None,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct V2RenderBackend {
    pub d2d_context: ID2D1DeviceContext,
    pub d_comp_device: IDCompositionDevice3,
    pub d_comp_target: IDCompositionTarget,
    pub d_comp_visual: IDCompositionVisual2,
    // The border only covers the edges of the window, so instead of one surface spanning the
    // whole canvas (~32 MB on a 4K monitor), we use a thin surface per edge. These are created
    // lazily by ensure_edges() and released while the border is hidden.
    //
    // I might be doing something wrong, but it seems like the surfaces MUST be dropped before
    // 'd_comp_target' or else we will have lingering resources, so the Drop impl does so explicitly.
    pub edges: Option<EdgeSurfaces>,
    // The size of the canvas the border is drawn on. It is sized to the monitor so that the
    // backend doesn't need to be resized every time the window resizes.
    pub surface_size: D2D_SIZE_U,
    pub border_bitmap: Option<ID2D1Bitmap1>,
    pub mask_bitmap: Option<ID2D1Bitmap1>,
    pub adapter_luid: LUID,
}

/// One surface per window edge, in the order top, bottom, left, right. The top and bottom
/// surfaces span the canvas width and the left and right surfaces span the canvas height, so only
/// their visuals' offsets need to change when the window resizes.
#[derive(Debug, Clone)]
pub struct EdgeSurfaces {
    visuals: [IDCompositionVisual2; 4],
    surfaces: [IDCompositionSurface; 4],
    thickness: u32,
}

/// Holds the ID2D1Multithread lock until dropped, so early returns can't leave it held.
struct D2DLock(ID2D1Multithread);

impl D2DLock {
    fn enter() -> WindowsCompatibleResult<Self> {
        let d2d_multithread: ID2D1Multithread = APP_STATE
            .render_factory
            .cast()
            .windows_context("d2d_multithread")?;
        unsafe { d2d_multithread.Enter() };
        Ok(Self(d2d_multithread))
    }
}

impl Drop for D2DLock {
    fn drop(&mut self) {
        unsafe { self.0.Leave() };
    }
}

#[derive(Debug, Clone)]
pub struct LegacyRenderBackend {
    pub render_target: ID2D1HwndRenderTarget,
}

impl RenderBackendConfig {
    pub fn to_render_backend(
        self,
        width: u32,
        height: u32,
        border_window: HWND,
        create_extra_bitmaps: bool,
    ) -> WindowsCompatibleResult<RenderBackend> {
        match self {
            RenderBackendConfig::V2 => Ok(RenderBackend::V2(V2RenderBackend::new(
                width,
                height,
                border_window,
                create_extra_bitmaps,
            )?)),
            RenderBackendConfig::Legacy => Ok(RenderBackend::Legacy(LegacyRenderBackend::new(
                border_window,
            )?)),
        }
    }
}

impl RenderBackend {
    pub fn resize(
        &mut self,
        width: u32,
        height: u32,
        create_extra_bitmaps: bool,
    ) -> WindowsCompatibleResult<()> {
        match self {
            RenderBackend::V2(backend) => {
                backend.resize(width, height, create_extra_bitmaps)?;
            }
            RenderBackend::Legacy(backend) => {
                backend.resize(width, height)?;
            }
            RenderBackend::None => {
                return Err(WindowsCompatibleError::Standalone(
                    StandaloneWindowsError::new(T_E_UNINIT, "render backend is None"),
                ));
            }
        }

        Ok(())
    }

    pub fn get_pixel_size(&self) -> WindowsCompatibleResult<D2D_SIZE_U> {
        match self {
            RenderBackend::V2(backend) => Ok(backend.surface_size),
            RenderBackend::Legacy(backend) => {
                let pixel_size = unsafe { backend.render_target.GetPixelSize() };
                Ok(pixel_size)
            }
            RenderBackend::None => Err(WindowsCompatibleError::Standalone(
                StandaloneWindowsError::new(T_E_UNINIT, "render backend is None"),
            )),
        }
    }

    pub fn supports_effects(&self) -> bool {
        !matches!(self, RenderBackend::Legacy(_) | RenderBackend::None)
    }
}

impl V2RenderBackend {
    pub fn new(
        width: u32,
        height: u32,
        border_window: HWND,
        create_extra_bitmaps: bool,
    ) -> WindowsCompatibleResult<Self> {
        let directx_devices_opt = APP_STATE.directx_devices.read().unwrap();
        let directx_devices = directx_devices_opt
            .as_ref()
            .context("could not get direct_devices")
            .to_windows_result(T_E_UNINIT)?;

        let d2d_context = unsafe {
            directx_devices
                .d2d_device
                .CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)
        }
        .windows_context("d2d_context")?;

        unsafe {
            d2d_context.SetAntialiasMode(D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);

            // Acquire a lock to prevent resource access conflict
            let d2d_multithread: ID2D1Multithread = APP_STATE
                .render_factory
                .cast()
                .windows_context("d2d_multithread")?;
            d2d_multithread.Enter();

            let dxgi_adapter = directx_devices
                .dxgi_device
                .GetAdapter()
                .windows_context("dxgi_adapter")?;

            // Not only does IDCompositionDevice3 not implement CreateTargetForHwnd, but you can't
            // even create one using DCompositionCreateDevice3 without casting (but you can with
            // IDCompositionDevice4... why?). Instead, we'll create IDCompositionDesktopDevice
            // first, which does implement CreateTargetForHwnd, then cast() it.
            let d_comp_desktop_device: IDCompositionDesktopDevice =
                DCompositionCreateDevice3(&directx_devices.dxgi_device)
                    .windows_context("d_comp_desktop_device")?;
            let d_comp_target = d_comp_desktop_device
                .CreateTargetForHwnd(border_window, true)
                .windows_context("d_comp_target")?;

            let d_comp_device: IDCompositionDevice3 = d_comp_desktop_device
                .cast()
                .windows_context("d_comp_desktop_device.cast()")?;

            let d_comp_visual = d_comp_device
                .CreateVisual()
                .windows_context("d_comp_visual")?;
            d_comp_target
                .SetRoot(&d_comp_visual)
                .windows_context("d_comp_target.SetRoot()")?;
            d_comp_device
                .Commit()
                .windows_context("d_comp_device.Commit()")?;

            d2d_multithread.Leave();

            let (border_bitmap_opt, mask_bitmap_opt) = if create_extra_bitmaps {
                let (border_bitmap, mask_bitmap) =
                    Self::create_extra_bitmaps(&d2d_context, width, height)?;
                (Some(border_bitmap), Some(mask_bitmap))
            } else {
                (None, None)
            };

            // The LUID identifies the GPU adapter this render backend was initialized with. It's
            // used to help determine when the primary GPU adapter of the system has changed.
            let adapter_desc = dxgi_adapter.GetDesc().windows_context("adapter_desc")?;
            let adapter_luid = adapter_desc.AdapterLuid;

            Ok(Self {
                d2d_context,
                d_comp_device,
                d_comp_target,
                d_comp_visual,
                edges: None,
                surface_size: D2D_SIZE_U { width, height },
                border_bitmap: border_bitmap_opt,
                mask_bitmap: mask_bitmap_opt,
                adapter_luid,
            })
        }
    }

    fn create_extra_bitmaps(
        d2d_context: &ID2D1DeviceContext,
        width: u32,
        height: u32,
    ) -> WindowsCompatibleResult<(ID2D1Bitmap1, ID2D1Bitmap1)> {
        // We need border_bitmap because you cannot directly apply effects on target_bitmap
        // due to its D2D1_BITMAP_OPTIONS, so we'll apply effects on border_bitmap instead
        let border_bitmap = unsafe {
            d2d_context.CreateBitmap(D2D_SIZE_U { width, height }, None, 0, &EXTRA_BITMAP_PROPS)
        }
        .windows_context("border_bitmap")?;

        // Aaaand we need yet another bitmap to serve as a mask for the border_bitmap
        let mask_bitmap = unsafe {
            d2d_context.CreateBitmap(D2D_SIZE_U { width, height }, None, 0, &EXTRA_BITMAP_PROPS)
        }
        .windows_context("mask_bitmap")?;

        Ok((border_bitmap, mask_bitmap))
    }

    pub fn release_references(&mut self) -> WindowsCompatibleResult<()> {
        unsafe {
            self.d2d_context.SetTarget(None);

            // Acquire a lock to prevent resource access conflict
            let d2d_multithread: ID2D1Multithread = APP_STATE
                .render_factory
                .cast()
                .windows_context("d2d_multithread")?;
            d2d_multithread.Enter();

            self.d_comp_visual
                .RemoveAllVisuals()
                .windows_context("d_comp_visual.RemoveAllVisuals()")?;
            self.d_comp_target
                .SetRoot(None)
                .windows_context("d_comp_target.SetRoot()")?;
            self.d_comp_device
                .Commit()
                .windows_context("d_comp_device.Commit()")?;

            d2d_multithread.Leave();
        }

        Ok(())
    }

    // NOTE: after updating resources, we also need to update anything that relies on references to
    // the resources (e.g. border effects)
    pub fn resize(
        &mut self,
        width: u32,
        height: u32,
        create_extra_bitmaps: bool,
    ) -> WindowsCompatibleResult<()> {
        // The edge surfaces are sized to the canvas, so drop them and let the next render recreate
        // them at the new size
        self.release_edges()
            .windows_context("could not release edge surfaces")?;
        unsafe { self.d2d_context.SetTarget(None) };
        self.border_bitmap = None;
        self.mask_bitmap = None;

        self.surface_size = D2D_SIZE_U { width, height };

        (self.border_bitmap, self.mask_bitmap) = if create_extra_bitmaps {
            let (border_bitmap, mask_bitmap) =
                Self::create_extra_bitmaps(&self.d2d_context, width, height)?;
            (Some(border_bitmap), Some(mask_bitmap))
        } else {
            (None, None)
        };

        Ok(())
    }
}

impl V2RenderBackend {
    /// Makes sure the edge surfaces exist and are at least 'thickness' pixels thick, (re)creating
    /// them if needed. The thickness is rounded up to avoid churn when e.g. the radius changes.
    pub fn ensure_edges(&mut self, thickness: u32) -> WindowsCompatibleResult<()> {
        if self
            .edges
            .as_ref()
            .is_some_and(|edges| edges.thickness >= thickness)
        {
            return Ok(());
        }

        let D2D_SIZE_U { width, height } = self.surface_size;
        let thickness = thickness.next_multiple_of(16).min(width).min(height).max(1);

        self.release_edges()?;

        let _lock = D2DLock::enter()?;
        let create_edge = |surface_width: u32, surface_height: u32| unsafe {
            let visual = self
                .d_comp_device
                .CreateVisual()
                .windows_context("edge visual")?;
            let surface = self
                .d_comp_device
                .CreateSurface(
                    surface_width,
                    surface_height,
                    DXGI_FORMAT_B8G8R8A8_UNORM,
                    DXGI_ALPHA_MODE_PREMULTIPLIED,
                )
                .windows_context("edge surface")?;
            visual
                .SetContent(&surface)
                .windows_context("edge visual.SetContent()")?;
            self.d_comp_visual
                .AddVisual(&visual, true, None)
                .windows_context("d_comp_visual.AddVisual()")?;
            WindowsCompatibleResult::Ok((visual, surface))
        };

        let (top_visual, top_surface) = create_edge(width, thickness)?;
        let (bottom_visual, bottom_surface) = create_edge(width, thickness)?;
        let (left_visual, left_surface) = create_edge(thickness, height)?;
        let (right_visual, right_surface) = create_edge(thickness, height)?;

        unsafe { self.d_comp_device.Commit() }.windows_context("d_comp_device.Commit()")?;

        self.edges = Some(EdgeSurfaces {
            visuals: [top_visual, bottom_visual, left_visual, right_visual],
            surfaces: [top_surface, bottom_surface, left_surface, right_surface],
            thickness,
        });

        Ok(())
    }

    /// Creates the bitmaps used for effects if they don't exist. Returns whether they were created,
    /// in which case anything referencing them (e.g. effect command lists) must be recreated too.
    pub fn ensure_extra_bitmaps(&mut self) -> WindowsCompatibleResult<bool> {
        if self.border_bitmap.is_some() && self.mask_bitmap.is_some() {
            return Ok(false);
        }

        let (border_bitmap, mask_bitmap) = Self::create_extra_bitmaps(
            &self.d2d_context,
            self.surface_size.width,
            self.surface_size.height,
        )?;
        self.border_bitmap = Some(border_bitmap);
        self.mask_bitmap = Some(mask_bitmap);

        Ok(true)
    }

    /// Releases the bitmaps used for effects (e.g. while the border is hidden). Note that effect
    /// command lists also hold references to them.
    pub fn release_extra_bitmaps(&mut self) {
        unsafe { self.d2d_context.SetTarget(None) };
        self.border_bitmap = None;
        self.mask_bitmap = None;
    }

    /// Releases the edge surfaces (e.g. while the border is hidden). They will be recreated by the
    /// next call to ensure_edges().
    pub fn release_edges(&mut self) -> WindowsCompatibleResult<()> {
        if self.edges.is_none() {
            return Ok(());
        }

        let _lock = D2DLock::enter()?;
        unsafe {
            self.d_comp_visual
                .RemoveAllVisuals()
                .windows_context("d_comp_visual.RemoveAllVisuals()")?;
            self.d_comp_device
                .Commit()
                .windows_context("d_comp_device.Commit()")?;
        }
        self.edges = None;

        Ok(())
    }

    /// Draws onto each edge surface. 'canvas' is the size of the area covered by the border
    /// window, and 'draw' is called once per edge with the transform and clip set up so that it
    /// can draw in canvas coordinates.
    pub fn draw_edges<F>(&self, canvas: D2D_SIZE_U, mut draw: F) -> WindowsCompatibleResult<()>
    where
        F: FnMut(&ID2D1DeviceContext) -> WindowsCompatibleResult<()>,
    {
        let edges = self
            .edges
            .as_ref()
            .context("could not get edge surfaces")
            .to_windows_result(T_E_UNINIT)?;

        let width = canvas.width.min(self.surface_size.width);
        let height = canvas.height.min(self.surface_size.height);

        // Split the canvas into non-overlapping strips so semi-transparent pixels aren't
        // composited twice. For windows smaller than two edges, the strips meet in the middle.
        let top = edges.thickness.min(height / 2);
        let bottom = edges.thickness.min(height - top);
        let left = edges.thickness.min(width / 2);
        let right = edges.thickness.min(width - left);
        let side_height = height - top - bottom;

        // (x, y, width, height) of each edge's visible area on the canvas
        let visible_rects = [
            (0, 0, width, top),
            (0, height - bottom, width, bottom),
            (0, top, left, side_height),
            (width - right, top, right, side_height),
        ];
        let surface_sizes = [
            (self.surface_size.width, edges.thickness),
            (self.surface_size.width, edges.thickness),
            (edges.thickness, self.surface_size.height),
            (edges.thickness, self.surface_size.height),
        ];

        let _lock = D2DLock::enter()?;
        let d2d_context = &self.d2d_context;

        for i in 0..4 {
            let (x, y, visible_width, visible_height) = visible_rects[i];
            let (surface_width, surface_height) = surface_sizes[i];

            unsafe {
                edges.visuals[i]
                    .SetOffsetX2(x as f32)
                    .windows_context("edge visual.SetOffsetX2()")?;
                edges.visuals[i]
                    .SetOffsetY2(y as f32)
                    .windows_context("edge visual.SetOffsetY2()")?;

                // The surface may live inside a larger atlas texture, so we must only touch the
                // area starting at 'offset'
                let mut offset = POINT::default();
                let dxgi_surface: IDXGISurface = edges.surfaces[i]
                    .BeginDraw(None, &mut offset)
                    .windows_context("edge surface.BeginDraw()")?;

                let draw_res = (|| {
                    let target_bitmap = d2d_context
                        .CreateBitmapFromDxgiSurface(&dxgi_surface, Some(&TARGET_BITMAP_PROPS))
                        .windows_context("target_bitmap")?;
                    d2d_context.SetTarget(&target_bitmap);
                    d2d_context.BeginDraw();

                    // Clear the whole surface, including any part beyond the window's edge
                    d2d_context.SetTransform(&Matrix3x2::identity());
                    d2d_context.PushAxisAlignedClip(
                        &D2D_RECT_F {
                            left: offset.x as f32,
                            top: offset.y as f32,
                            right: (offset.x + surface_width as i32) as f32,
                            bottom: (offset.y + surface_height as i32) as f32,
                        },
                        D2D1_ANTIALIAS_MODE_ALIASED,
                    );
                    d2d_context.Clear(None);
                    d2d_context.PopAxisAlignedClip();

                    let mut draw_res = Ok(());
                    if visible_width > 0 && visible_height > 0 {
                        // Map canvas coordinates onto this edge's area of the surface
                        d2d_context.SetTransform(&Matrix3x2::translation(
                            (offset.x - x as i32) as f32,
                            (offset.y - y as i32) as f32,
                        ));
                        d2d_context.PushAxisAlignedClip(
                            &D2D_RECT_F {
                                left: x as f32,
                                top: y as f32,
                                right: (x + visible_width) as f32,
                                bottom: (y + visible_height) as f32,
                            },
                            D2D1_ANTIALIAS_MODE_ALIASED,
                        );
                        draw_res = draw(d2d_context);
                        d2d_context.PopAxisAlignedClip();
                        d2d_context.SetTransform(&Matrix3x2::identity());
                    }

                    // Always end the draw, even if 'draw' failed, so the context stays usable
                    d2d_context.EndDraw(None, None)?;
                    draw_res
                })();

                d2d_context.SetTarget(None);
                edges.surfaces[i]
                    .EndDraw()
                    .windows_context("edge surface.EndDraw()")?;
                draw_res?;
            }
        }

        unsafe { self.d_comp_device.Commit() }.windows_context("d_comp_device.Commit()")?;

        Ok(())
    }
}

impl Drop for V2RenderBackend {
    fn drop(&mut self) {
        self.release_references()
            .context("could not drop V2RenderBackend: could not release renderer references")
            .log_if_err();

        // Like mentioned in a comment near the struct declaration, the edge surfaces MUST be
        // dropped before other struct fields, so we will do it here.
        self.edges = None;
    }
}

impl LegacyRenderBackend {
    pub fn new(border_window: HWND) -> WindowsCompatibleResult<Self> {
        let render_target_properties = D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_UNKNOWN,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: 96.0,
            dpiY: 96.0,
            ..Default::default()
        };
        let hwnd_render_target_properties = D2D1_HWND_RENDER_TARGET_PROPERTIES {
            hwnd: border_window,
            pixelSize: Default::default(),
            presentOptions: D2D1_PRESENT_OPTIONS_RETAIN_CONTENTS | D2D1_PRESENT_OPTIONS_IMMEDIATELY,
        };

        unsafe {
            let render_target = APP_STATE
                .render_factory
                .CreateHwndRenderTarget(&render_target_properties, &hwnd_render_target_properties)
                .windows_context("render_target")?;

            render_target.SetAntialiasMode(D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);

            Ok(Self { render_target })
        }
    }

    pub fn resize(&self, width: u32, height: u32) -> WindowsCompatibleResult<()> {
        let pixel_size = D2D_SIZE_U { width, height };
        unsafe { self.render_target.Resize(&pixel_size) }
            .windows_context("could not resize render_target")?;

        Ok(())
    }
}
