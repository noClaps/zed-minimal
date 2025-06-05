use std::ops::Range;

use gpui::{FontStyle, FontWeight, HighlightStyle};
use rpc::proto::{self, documentation};

#[derive(Debug)]
pub struct SignatureHelp {
    pub label: String,
    pub highlights: Vec<(Range<usize>, HighlightStyle)>,
    pub(super) original_data: lsp::SignatureHelp,
}

impl SignatureHelp {
    pub fn new(help: lsp::SignatureHelp) -> Option<Self> {
        let function_options_count = help.signatures.len();

        let signature_information = help
            .active_signature
            .and_then(|active_signature| help.signatures.get(active_signature as usize))
            .or_else(|| help.signatures.first())?;

        let str_for_join = ", ";
        let parameter_length = signature_information
            .parameters
            .as_ref()
            .map_or(0, |parameters| parameters.len());
        let mut highlight_start = 0;
        let (strings, mut highlights): (Vec<_>, Vec<_>) = signature_information
            .parameters
            .as_ref()?
            .iter()
            .enumerate()
            .map(|(i, parameter_information)| {
                let label = match parameter_information.label.clone() {
                    lsp::ParameterLabel::Simple(string) => string,
                    lsp::ParameterLabel::LabelOffsets(offset) => signature_information
                        .label
                        .chars()
                        .skip(offset[0] as usize)
                        .take((offset[1] - offset[0]) as usize)
                        .collect::<String>(),
                };
                let label_length = label.len();

                let highlights = help.active_parameter.and_then(|active_parameter| {
                    if i == active_parameter as usize {
                        Some((
                            highlight_start..(highlight_start + label_length),
                            HighlightStyle {
                                font_weight: Some(FontWeight::EXTRA_BOLD),
                                ..Default::default()
                            },
                        ))
                    } else {
                        None
                    }
                });

                if i != parameter_length {
                    highlight_start += label_length + str_for_join.len();
                }

                (label, highlights)
            })
            .unzip();

        if strings.is_empty() {
            None
        } else {
            let mut label = strings.join(str_for_join);

            if function_options_count >= 2 {
                let suffix = format!("(+{} overload)", function_options_count - 1);
                let highlight_start = label.len() + 1;
                highlights.push(Some((
                    highlight_start..(highlight_start + suffix.len()),
                    HighlightStyle {
                        font_style: Some(FontStyle::Italic),
                        ..Default::default()
                    },
                )));
                label.push(' ');
                label.push_str(&suffix);
            };

            Some(Self {
                label,
                highlights: highlights.into_iter().flatten().collect(),
                original_data: help,
            })
        }
    }
}

pub fn lsp_to_proto_signature(lsp_help: lsp::SignatureHelp) -> proto::SignatureHelp {
    proto::SignatureHelp {
        signatures: lsp_help
            .signatures
            .into_iter()
            .map(|signature| proto::SignatureInformation {
                label: signature.label,
                documentation: signature.documentation.map(lsp_to_proto_documentation),
                parameters: signature
                    .parameters
                    .unwrap_or_default()
                    .into_iter()
                    .map(|parameter_info| proto::ParameterInformation {
                        label: Some(match parameter_info.label {
                            lsp::ParameterLabel::Simple(label) => {
                                proto::parameter_information::Label::Simple(label)
                            }
                            lsp::ParameterLabel::LabelOffsets(offsets) => {
                                proto::parameter_information::Label::LabelOffsets(
                                    proto::LabelOffsets {
                                        start: offsets[0],
                                        end: offsets[1],
                                    },
                                )
                            }
                        }),
                        documentation: parameter_info.documentation.map(lsp_to_proto_documentation),
                    })
                    .collect(),
                active_parameter: signature.active_parameter,
            })
            .collect(),
        active_signature: lsp_help.active_signature,
        active_parameter: lsp_help.active_parameter,
    }
}

fn lsp_to_proto_documentation(documentation: lsp::Documentation) -> proto::Documentation {
    proto::Documentation {
        content: Some(match documentation {
            lsp::Documentation::String(string) => proto::documentation::Content::Value(string),
            lsp::Documentation::MarkupContent(content) => {
                proto::documentation::Content::MarkupContent(proto::MarkupContent {
                    is_markdown: matches!(content.kind, lsp::MarkupKind::Markdown),
                    value: content.value,
                })
            }
        }),
    }
}

pub fn proto_to_lsp_signature(proto_help: proto::SignatureHelp) -> lsp::SignatureHelp {
    lsp::SignatureHelp {
        signatures: proto_help
            .signatures
            .into_iter()
            .map(|signature| lsp::SignatureInformation {
                label: signature.label,
                documentation: signature.documentation.and_then(proto_to_lsp_documentation),
                parameters: Some(
                    signature
                        .parameters
                        .into_iter()
                        .filter_map(|parameter_info| {
                            Some(lsp::ParameterInformation {
                                label: match parameter_info.label? {
                                    proto::parameter_information::Label::Simple(string) => {
                                        lsp::ParameterLabel::Simple(string)
                                    }
                                    proto::parameter_information::Label::LabelOffsets(offsets) => {
                                        lsp::ParameterLabel::LabelOffsets([
                                            offsets.start,
                                            offsets.end,
                                        ])
                                    }
                                },
                                documentation: parameter_info
                                    .documentation
                                    .and_then(proto_to_lsp_documentation),
                            })
                        })
                        .collect(),
                ),
                active_parameter: signature.active_parameter,
            })
            .collect(),
        active_signature: proto_help.active_signature,
        active_parameter: proto_help.active_parameter,
    }
}

fn proto_to_lsp_documentation(documentation: proto::Documentation) -> Option<lsp::Documentation> {
    {
        Some(match documentation.content? {
            documentation::Content::Value(string) => lsp::Documentation::String(string),
            documentation::Content::MarkupContent(markup) => {
                lsp::Documentation::MarkupContent(if markup.is_markdown {
                    lsp::MarkupContent {
                        kind: lsp::MarkupKind::Markdown,
                        value: markup.value,
                    }
                } else {
                    lsp::MarkupContent {
                        kind: lsp::MarkupKind::PlainText,
                        value: markup.value,
                    }
                })
            }
        })
    }
}
