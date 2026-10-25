//! DirectDraw (`ddraw`), and Direct3D 8: the COM interfaces the game calls (`d3d8`), the reference software
//! rasterizer (`raster`) and the Metal renderer that does the same work on the GPU (`metal`).

pub mod d3d8;
pub mod ddraw;
pub mod metal;
pub mod raster;
