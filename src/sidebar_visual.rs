//! The separator is a single opaque surface. Moving it changes only a
//! compositor offset, so its pixels cannot be copied into a sibling HWND.
use crate::win::{H, hwnd};
use windows::{
    Win32::{
        Foundation::{E_POINTER, HMODULE, HWND, POINT},
        Graphics::{
            Direct3D::D3D_DRIVER_TYPE_HARDWARE,
            Direct3D11::*,
            DirectComposition::*,
            Dxgi::{Common::*, IDXGIDevice, IDXGISurface},
        },
    },
    core::{Interface, Result},
};

pub(super) struct Edge {
    pub(super) context: ID3D11DeviceContext,
    pub(super) device: IDCompositionDevice,
    target: IDCompositionTarget,
    root: IDCompositionVisual,
    visual: IDCompositionVisual,
    scene: Option<super::scene::Scene>,
    surface: Option<(i32, i32, u32)>,
    shown: bool,
    position: Option<(i32, i32)>,
}

impl Edge {
    pub(super) fn new(parent: H) -> Result<Self> {
        unsafe {
            let (mut device, mut context) = (None, None);
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
            let dxgi: IDXGIDevice = device.ok_or(E_POINTER)?.cast()?;
            let device: IDCompositionDevice = DCompositionCreateDevice(&dxgi)?;
            let target = device.CreateTargetForHwnd(HWND(hwnd(parent)), true)?;
            let visual = device.CreateVisual()?;
            let root = device.CreateVisual()?;
            root.AddVisual(&visual, true, None::<&IDCompositionVisual>)?;
            Ok(Self {
                context: context.ok_or(E_POINTER)?,
                device,
                target,
                root,
                visual,
                scene: None,
                surface: None,
                shown: false,
                position: None,
            })
        }
    }

    pub(super) fn begin_drag(
        &mut self,
        cfg: super::Layout,
        content: super::scene::Content,
        side: i32,
    ) -> Result<()> {
        let mut scene = super::scene::Scene::new(self, cfg, content)?;
        scene.draw(side)?;
        unsafe {
            self.root.AddVisual(&scene.group, false, &self.visual)?;
            if let Some(old) = &self.scene {
                self.root.RemoveVisual(&old.group)?;
            }
            self.device.Commit()?;
        }
        self.scene = Some(scene);
        Ok(())
    }
    pub(super) fn end_drag(&mut self) -> Result<()> {
        if let Some(scene) = self.scene.take() {
            unsafe {
                self.root.RemoveVisual(&scene.group)?;
                self.device.Commit()?;
            }
        }
        Ok(())
    }
    pub(super) fn update_pane(&self, h: H, p: &super::scene::Pixels) -> Result<()> {
        if let Some(scene) = &self.scene {
            scene.update_pane(self, h, p)?;
        }
        Ok(())
    }

    pub(super) fn place(
        &mut self,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        color: u32,
    ) -> Result<()> {
        let shape = (width, height, color);
        if let Some(scene) = &mut self.scene {
            scene.draw(x + width)?;
        }
        if self.shown && self.position == Some((x, y)) && self.surface == Some(shape) {
            return Ok(());
        }
        unsafe {
            if self.surface != Some(shape) {
                let surface = self.device.CreateSurface(
                    width as _,
                    height as _,
                    DXGI_FORMAT_B8G8R8A8_UNORM,
                    DXGI_ALPHA_MODE_IGNORE,
                )?;
                let mut offset = POINT::default();
                let image: IDXGISurface = surface.BeginDraw(None, &mut offset)?;
                let texture = image.cast::<ID3D11Texture2D>();
                if let Ok(texture) = &texture {
                    let bgra =
                        0xff000000 | ((color & 0xff) << 16) | (color & 0xff00) | (color >> 16);
                    let pixels = vec![bgra; width as usize * height as usize];
                    let region = D3D11_BOX {
                        left: offset.x as _,
                        top: offset.y as _,
                        front: 0,
                        right: (offset.x + width) as _,
                        bottom: (offset.y + height) as _,
                        back: 1,
                    };
                    self.context.UpdateSubresource(
                        texture,
                        0,
                        Some(&region),
                        pixels.as_ptr().cast(),
                        (width * 4) as _,
                        0,
                    );
                }
                surface.EndDraw()?;
                texture?;
                self.visual.SetContent(&surface)?;
                self.surface = Some(shape);
            }
            self.visual.SetOffsetX2(x as f32)?;
            self.visual.SetOffsetY2(y as f32)?;
            if !self.shown {
                self.target.SetRoot(&self.root)?;
            }
            self.device.Commit()?;
            self.shown = true;
            self.position = Some((x, y));
        }
        Ok(())
    }
    pub(super) fn hide(&mut self) -> Result<()> {
        if self.shown {
            unsafe {
                self.target.SetRoot(None::<&IDCompositionVisual>)?;
                self.device.Commit()?;
            }
            self.shown = false;
        }
        Ok(())
    }
}
