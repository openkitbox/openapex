//! ACP content conversion kept independent from transport and event orchestration.

use agent_client_protocol::v1::schema::{ContentBlock, EmbeddedResourceResource};

use super::internal;

/// The safe, display-preserving lowering of one ACP prompt block.
pub(crate) enum PromptBlock<'prompt> {
    Text(&'prompt str),
    Image {
        data: &'prompt str,
        mime_type: &'prompt str,
    },
    ResourceLink,
    EmbeddedText(&'prompt str),
}

pub(crate) fn resource_link(
    uri: &str,
    name: Option<&str>,
    description: Option<&str>,
) -> ContentBlock {
    let mut value = agent_client_protocol::v1::schema::ResourceLink::new(name.unwrap_or(uri), uri);
    if let Some(description) = description {
        value = value.description(description);
    }
    ContentBlock::ResourceLink(value)
}

pub(crate) fn prompt_block(
    block: &ContentBlock,
) -> Result<PromptBlock<'_>, agent_client_protocol::Error> {
    match block {
        ContentBlock::Text(content) => Ok(PromptBlock::Text(&content.text)),
        ContentBlock::Image(content) => Ok(PromptBlock::Image {
            data: &content.data,
            mime_type: &content.mime_type,
        }),
        ContentBlock::ResourceLink(_) => Ok(PromptBlock::ResourceLink),
        ContentBlock::Resource(resource) => match &resource.resource {
            EmbeddedResourceResource::TextResourceContents(content) => {
                Ok(PromptBlock::EmbeddedText(&content.text))
            }
            EmbeddedResourceResource::BlobResourceContents(_) => {
                Err(internal("binary embedded resources are not supported"))
            }
            _ => Err(internal("unsupported embedded resource")),
        },
        ContentBlock::Audio(_) => Err(internal("audio prompts are not supported")),
        _ => Err(internal("unsupported prompt content block")),
    }
}
