//! Image tools (port of `OpenOrca.Tools.ImageGeneration`): `generate_image` and `edit_image`
//! call an OpenAI-compatible images API; `crop_image` and `resize_image` work on local files
//! with the `image` crate, built for PNG, JPEG, WebP, GIF and BMP only.

pub mod edit_local;
pub mod formats;
pub mod generate;

use std::sync::Arc;

use orca_abstractions::Tool;
use orca_core::config::ImageGenerationConfig;

pub use edit_local::{CropImageTool, ResizeImageTool};
pub use generate::{
    EditImageTool, GenerateImageTool, SharedUncensored, active_image_model, resolve_image_model,
};

/// The image tools, in the C# registration order.
pub fn all_tools(
    config: &ImageGenerationConfig,
    uncensored: Option<SharedUncensored>,
) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(CropImageTool),
        Arc::new(EditImageTool::new(config.clone())),
        Arc::new(GenerateImageTool::new(config.clone(), uncensored)),
        Arc::new(ResizeImageTool),
    ]
}
