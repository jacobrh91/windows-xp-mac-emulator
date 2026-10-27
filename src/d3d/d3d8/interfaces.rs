//! The Direct3D 8 interfaces: their methods in vtable order (with argument counts, `this`
//! included), and their IIDs.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Iface {
    D3d,
    Device,
    Texture,
    Surface,
    VertexBuffer,
    IndexBuffer,
    SwapChain,
}

pub(super) const ALL_IFACES: [Iface; 7] = [
    Iface::D3d,
    Iface::Device,
    Iface::Texture,
    Iface::Surface,
    Iface::VertexBuffer,
    Iface::IndexBuffer,
    Iface::SwapChain,
];

impl Iface {
    pub(super) fn name(self) -> &'static str {
        match self {
            Iface::D3d => "IDirect3D8",
            Iface::Device => "IDirect3DDevice8",
            Iface::Texture => "IDirect3DTexture8",
            Iface::Surface => "IDirect3DSurface8",
            Iface::VertexBuffer => "IDirect3DVertexBuffer8",
            Iface::IndexBuffer => "IDirect3DIndexBuffer8",
            Iface::SwapChain => "IDirect3DSwapChain8",
        }
    }

    /// Methods in vtable order, with their argument counts including `this`.
    pub(super) fn methods(self) -> &'static [(&'static str, usize)] {
        match self {
            Iface::D3d => D3D_METHODS,
            Iface::Device => DEVICE_METHODS,
            Iface::Texture => TEXTURE_METHODS,
            Iface::Surface => SURFACE_METHODS,
            Iface::VertexBuffer => VB_METHODS,
            Iface::IndexBuffer => IB_METHODS,
            Iface::SwapChain => SWAPCHAIN_METHODS,
        }
    }
}

pub(super) const D3D_METHODS: &[(&str, usize)] = &[
    ("QueryInterface", 3),
    ("AddRef", 1),
    ("Release", 1),
    ("RegisterSoftwareDevice", 2),
    ("GetAdapterCount", 1),
    ("GetAdapterIdentifier", 4),
    ("GetAdapterModeCount", 2),
    ("EnumAdapterModes", 4),
    ("GetAdapterDisplayMode", 3),
    ("CheckDeviceType", 6),
    ("CheckDeviceFormat", 7),
    ("CheckDeviceMultiSampleType", 6),
    ("CheckDepthStencilMatch", 6),
    ("GetDeviceCaps", 4),
    ("GetAdapterMonitor", 2),
    ("CreateDevice", 7),
];

pub(super) const DEVICE_METHODS: &[(&str, usize)] = &[
    ("QueryInterface", 3),
    ("AddRef", 1),
    ("Release", 1),
    ("TestCooperativeLevel", 1),
    ("GetAvailableTextureMem", 1),
    ("ResourceManagerDiscardBytes", 2),
    ("GetDirect3D", 2),
    ("GetDeviceCaps", 2),
    ("GetDisplayMode", 2),
    ("GetCreationParameters", 2),
    ("SetCursorProperties", 4),
    ("SetCursorPosition", 4),
    ("ShowCursor", 2),
    ("CreateAdditionalSwapChain", 3),
    ("Reset", 2),
    ("Present", 5),
    ("GetBackBuffer", 4),
    ("GetRasterStatus", 2),
    ("SetGammaRamp", 3),
    ("GetGammaRamp", 2),
    ("CreateTexture", 8),
    ("CreateVolumeTexture", 9),
    ("CreateCubeTexture", 7),
    ("CreateVertexBuffer", 6),
    ("CreateIndexBuffer", 6),
    ("CreateRenderTarget", 7),
    ("CreateDepthStencilSurface", 6),
    ("CreateImageSurface", 5),
    ("CopyRects", 6),
    ("UpdateTexture", 3),
    ("GetFrontBuffer", 2),
    ("SetRenderTarget", 3),
    ("GetRenderTarget", 2),
    ("GetDepthStencilSurface", 2),
    ("BeginScene", 1),
    ("EndScene", 1),
    ("Clear", 7),
    ("SetTransform", 3),
    ("GetTransform", 3),
    ("MultiplyTransform", 3),
    ("SetViewport", 2),
    ("GetViewport", 2),
    ("SetMaterial", 2),
    ("GetMaterial", 2),
    ("SetLight", 3),
    ("GetLight", 3),
    ("LightEnable", 3),
    ("GetLightEnable", 3),
    ("SetClipPlane", 3),
    ("GetClipPlane", 3),
    ("SetRenderState", 3),
    ("GetRenderState", 3),
    ("BeginStateBlock", 1),
    ("EndStateBlock", 2),
    ("ApplyStateBlock", 2),
    ("CaptureStateBlock", 2),
    ("DeleteStateBlock", 2),
    ("CreateStateBlock", 3),
    ("SetClipStatus", 2),
    ("GetClipStatus", 2),
    ("GetTexture", 3),
    ("SetTexture", 3),
    ("GetTextureStageState", 4),
    ("SetTextureStageState", 4),
    ("ValidateDevice", 2),
    ("GetInfo", 4),
    ("SetPaletteEntries", 3),
    ("GetPaletteEntries", 3),
    ("SetCurrentTexturePalette", 2),
    ("GetCurrentTexturePalette", 2),
    ("DrawPrimitive", 4),
    ("DrawIndexedPrimitive", 6),
    ("DrawPrimitiveUP", 5),
    ("DrawIndexedPrimitiveUP", 9),
    ("ProcessVertices", 6),
    ("CreateVertexShader", 5),
    ("SetVertexShader", 2),
    ("GetVertexShader", 2),
    ("DeleteVertexShader", 2),
    ("SetVertexShaderConstant", 4),
    ("GetVertexShaderConstant", 4),
    ("GetVertexShaderDeclaration", 4),
    ("GetVertexShaderFunction", 4),
    ("SetStreamSource", 4),
    ("GetStreamSource", 4),
    ("SetIndices", 3),
    ("GetIndices", 3),
    ("CreatePixelShader", 3),
    ("SetPixelShader", 2),
    ("GetPixelShader", 2),
    ("DeletePixelShader", 2),
    ("SetPixelShaderConstant", 4),
    ("GetPixelShaderConstant", 4),
    ("GetPixelShaderFunction", 4),
    ("DrawRectPatch", 4),
    ("DrawTriPatch", 4),
    ("DeletePatch", 2),
];

pub(super) const RESOURCE_HEAD: [(&str, usize); 11] = [
    ("QueryInterface", 3),
    ("AddRef", 1),
    ("Release", 1),
    ("GetDevice", 2),
    ("SetPrivateData", 5),
    ("GetPrivateData", 4),
    ("FreePrivateData", 2),
    ("SetPriority", 2),
    ("GetPriority", 1),
    ("PreLoad", 1),
    ("GetType", 1),
];

pub(super) const TEXTURE_METHODS: &[(&str, usize)] = &[
    RESOURCE_HEAD[0],
    RESOURCE_HEAD[1],
    RESOURCE_HEAD[2],
    RESOURCE_HEAD[3],
    RESOURCE_HEAD[4],
    RESOURCE_HEAD[5],
    RESOURCE_HEAD[6],
    RESOURCE_HEAD[7],
    RESOURCE_HEAD[8],
    RESOURCE_HEAD[9],
    RESOURCE_HEAD[10],
    ("SetLOD", 2),
    ("GetLOD", 1),
    ("GetLevelCount", 1),
    ("GetLevelDesc", 3),
    ("GetSurfaceLevel", 3),
    ("LockRect", 5),
    ("UnlockRect", 2),
    ("AddDirtyRect", 2),
];

pub(super) const SURFACE_METHODS: &[(&str, usize)] = &[
    ("QueryInterface", 3),
    ("AddRef", 1),
    ("Release", 1),
    ("GetDevice", 2),
    ("SetPrivateData", 5),
    ("GetPrivateData", 4),
    ("FreePrivateData", 2),
    ("GetContainer", 3),
    ("GetDesc", 2),
    ("LockRect", 4),
    ("UnlockRect", 1),
];

pub(super) const VB_METHODS: &[(&str, usize)] = &[
    RESOURCE_HEAD[0],
    RESOURCE_HEAD[1],
    RESOURCE_HEAD[2],
    RESOURCE_HEAD[3],
    RESOURCE_HEAD[4],
    RESOURCE_HEAD[5],
    RESOURCE_HEAD[6],
    RESOURCE_HEAD[7],
    RESOURCE_HEAD[8],
    RESOURCE_HEAD[9],
    RESOURCE_HEAD[10],
    ("Lock", 5),
    ("Unlock", 1),
    ("GetDesc", 2),
];

pub(super) const IB_METHODS: &[(&str, usize)] = VB_METHODS;

pub(super) const SWAPCHAIN_METHODS: &[(&str, usize)] = &[
    ("QueryInterface", 3),
    ("AddRef", 1),
    ("Release", 1),
    ("Present", 5),
    ("GetBackBuffer", 4),
];

pub(super) fn guid(d1: u32, d2: u16, d3: u16, d4: [u8; 8]) -> [u8; 16] {
    let mut g = [0u8; 16];
    g[0..4].copy_from_slice(&d1.to_le_bytes());
    g[4..6].copy_from_slice(&d2.to_le_bytes());
    g[6..8].copy_from_slice(&d3.to_le_bytes());
    g[8..16].copy_from_slice(&d4);
    g
}

pub(super) fn iid_of(i: Iface) -> [u8; 16] {
    match i {
        Iface::D3d => guid(
            0x1DD9E8DA,
            0x1C77,
            0x4D40,
            [0xB0, 0xCF, 0x98, 0xFE, 0xFD, 0xFF, 0x95, 0x12],
        ),
        Iface::Device => guid(
            0x7385E5DF,
            0x8FE8,
            0x41D5,
            [0x86, 0xB6, 0xD7, 0xB4, 0x85, 0x47, 0xB6, 0xCF],
        ),
        Iface::Texture => guid(
            0xE4CDD575,
            0x2866,
            0x4F01,
            [0xB1, 0x2E, 0x7E, 0xEC, 0xE1, 0xEC, 0x93, 0x58],
        ),
        Iface::Surface => guid(
            0xB96EEBCA,
            0xB326,
            0x4EA5,
            [0x88, 0x2F, 0x2F, 0xF5, 0xBA, 0xE0, 0x21, 0xDD],
        ),
        Iface::VertexBuffer => guid(
            0x8AEEEAC7,
            0x05F9,
            0x44D4,
            [0xB5, 0x91, 0x00, 0x0B, 0x0D, 0xF1, 0xCB, 0x95],
        ),
        Iface::IndexBuffer => guid(
            0x0E689C9A,
            0x053D,
            0x44A0,
            [0x9D, 0x92, 0xDB, 0x0E, 0x3D, 0x75, 0x0F, 0x86],
        ),
        Iface::SwapChain => guid(
            0x928C088B,
            0x76B9,
            0x4C6B,
            [0xA5, 0x36, 0xA5, 0x90, 0x85, 0x38, 0x76, 0xCD],
        ),
    }
}

pub(super) const IID_IUNKNOWN: [u8; 16] = [0, 0, 0, 0, 0, 0, 0, 0, 0xC0, 0, 0, 0, 0, 0, 0, 0x46];
pub(super) const IID_BASE_TEXTURE: [u8; 16] = [
    0xFA, 0x1C, 0x21, 0xB4, 0xB9, 0x51, 0x9F, 0x4A, 0xAB, 0x78, 0xDB, 0x99, 0xB2, 0xBB, 0x67, 0x8E,
];
pub(super) const IID_RESOURCE: [u8; 16] = [
    0x7B, 0xBB, 0x36, 0x1B, 0xB7, 0x09, 0x0A, 0x41, 0xB4, 0x45, 0x7D, 0x14, 0x30, 0xD7, 0xB3, 0x3E,
];
