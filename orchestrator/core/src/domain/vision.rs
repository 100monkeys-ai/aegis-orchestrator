// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Image generation provider (AEGIS ADR-046 D1, ADR-143 I1)
//!
//! [`VisionProvider`] abstracts a vendor's image creation API, as
//! [`crate::domain::llm::LLMProvider`] abstracts its chat completions. It is a
//! separate trait, never methods on `LLMProvider` (ADR-046 Alternative 2): not
//! every model generates images, and an alias that does is never an agent's
//! language model.
//!
//! A node's model alias carrying the capability
//! [`IMAGE_GENERATION_CAPABILITY`] is bound to an adapter of this trait when
//! the provider registry is built, and to no `LLMProvider`.

use async_trait::async_trait;
use std::sync::Arc;
use thiserror::Error;

/// The value of `ModelConfig.capabilities` that makes an alias an image
/// generation model.
pub const IMAGE_GENERATION_CAPABILITY: &str = "image_generation";

/// The refusal of an agent manifest whose `spec.runtime.model` is an alias
/// that generates images.
pub fn agent_model_generates_images(alias: &str) -> String {
    format!("'{alias}' generates images; an agent's model must be a language model")
}

/// The refusal of an image generation call on a node with no image
/// generation alias.
pub const NO_IMAGE_GENERATION_MODEL: &str = "this node has no image generation model";

/// The most images one image generation call asks for.
pub const IMAGE_COUNT_MAX: u8 = 4;

/// The node's image generation models: its aliases with the capability
/// [`IMAGE_GENERATION_CAPABILITY`], each bound to a [`VisionProvider`].
pub trait ImageGenerationModels: Send + Sync {
    /// The aliases, in the order the node's configuration lists them; the
    /// first is the default of a call that names none.
    fn image_generation_aliases(&self) -> Vec<String>;

    /// The provider an alias is bound to; `None` for an alias that is not an
    /// image generation model.
    fn image_provider(&self, alias: &str) -> Option<Arc<dyn VisionProvider>>;
}

/// What an image generation request asks for beside its prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageOptions {
    /// The provider's size string (for example `1024x1024`); the provider's
    /// default when `None`.
    pub size: Option<String>,
    /// How many images to generate.
    pub count: u8,
}

impl Default for ImageOptions {
    fn default() -> Self {
        Self {
            size: None,
            count: 1,
        }
    }
}

/// One image: its bytes and their content type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub bytes: Vec<u8>,
    pub content_type: String,
}

/// The area of an image an edit may change: an image of the same size whose
/// transparent pixels mark it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mask {
    pub bytes: Vec<u8>,
}

/// The images a provider answered, in its order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageResponse {
    pub images: Vec<Image>,
}

#[derive(Debug, Error)]
pub enum VisionError {
    /// The provider refused the request as sent (HTTP 400).
    #[error("invalid image request: {0}")]
    InvalidInput(String),
    /// The provider refused the credential (HTTP 401 or 403).
    #[error("image provider authentication failed: {0}")]
    Authentication(String),
    /// The provider limited the request rate (HTTP 429).
    #[error("image provider rate limit exceeded")]
    RateLimit,
    /// Any other failure the provider answered, or an answer that is not one.
    #[error("image provider error: {0}")]
    Provider(String),
    /// The request did not reach the provider, or its answer did not arrive.
    #[error("image provider network error: {0}")]
    Network(String),
    /// The adapter does not offer the operation.
    #[error("{0}")]
    Unsupported(String),
}

/// A vendor's image creation API.
#[async_trait]
pub trait VisionProvider: Send + Sync {
    /// Generate `options.count` images for `prompt`.
    async fn generate_image(
        &self,
        prompt: &str,
        options: &ImageOptions,
    ) -> Result<ImageResponse, VisionError>;

    /// Edit `image` by `prompt`, inside `mask` where one is given.
    async fn edit_image(
        &self,
        image: &Image,
        prompt: &str,
        mask: Option<&Mask>,
    ) -> Result<ImageResponse, VisionError>;
}
