use super::*;
use pretty_assertions::assert_eq;

#[test]
fn notebook_read_renders_cells_and_text_outputs() {
    let notebook = serde_json::json!({
        "metadata": { "language_info": { "name": "python" } },
        "cells": [{
            "id": "intro",
            "cell_type": "markdown",
            "source": ["# Heading\n", "Body"]
        }, {
            "id": "code",
            "cell_type": "code",
            "source": ["print('hi')"],
            "outputs": [{ "output_type": "stream", "text": ["hi\n"] }]
        }]
    });

    let output =
        render_notebook(&notebook.to_string(), "example.ipynb", 2_000).expect("render notebook");

    assert_eq!(output.content.len(), 1);
    let FunctionCallOutputContentItem::InputText { text } = &output.content[0] else {
        panic!("expected text output");
    };
    assert!(text.contains("<cell id=\"intro\"><cell_type>markdown</cell_type># Heading"));
    assert!(text.contains("<cell id=\"code\">print('hi')</cell id=\"code\">\n\nhi"));
    assert_eq!(
        output.event_summary,
        "Read notebook `example.ipynb` (2 cells)"
    );
}

#[test]
fn notebook_read_rejects_invalid_json() {
    assert!(render_notebook("not json", "example.ipynb", 2_000).is_err());
}

#[test]
fn notebook_read_returns_embedded_png_outputs() {
    let notebook = serde_json::json!({
        "metadata": { "language_info": { "name": "python" } },
        "cells": [{
            "id": "plot",
            "cell_type": "code",
            "source": ["display(plot)"],
            "outputs": [{
                "output_type": "display_data",
                "data": {
                    "text/plain": ["<plot>"],
                    "image/png": "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg=="
                }
            }]
        }]
    });

    let output =
        render_notebook(&notebook.to_string(), "plot.ipynb", 2_000).expect("render notebook image");

    assert_eq!(output.content.len(), 2);
    assert!(matches!(
        output.content[1],
        FunctionCallOutputContentItem::InputImage { .. }
    ));
}

#[test]
fn notebook_image_size_is_not_counted_as_text_output() {
    let notebook = serde_json::json!({
        "metadata": { "language_info": { "name": "python" } },
        "cells": [{
            "id": "plot",
            "cell_type": "code",
            "source": ["display(plot)"],
            "outputs": [{
                "output_type": "display_data",
                "data": {
                    "text/plain": ["<plot>"],
                    "application/json": "x".repeat(MAX_CELL_OUTPUT_CHARS + 1),
                    "image/png": "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg=="
                }
            }]
        }]
    });

    let output =
        render_notebook(&notebook.to_string(), "plot.ipynb", 2_000).expect("render notebook image");

    assert!(matches!(
        output.content.last(),
        Some(FunctionCallOutputContentItem::InputImage { .. })
    ));
}

#[test]
fn notebook_read_rejects_more_than_the_context_safe_image_count() {
    let image_output = serde_json::json!({
        "output_type": "display_data",
        "data": {
            "image/png": "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg=="
        }
    });
    let notebook = serde_json::json!({
        "metadata": { "language_info": { "name": "python" } },
        "cells": [{
            "id": "plots",
            "cell_type": "code",
            "source": ["display(plots)"],
            "outputs": vec![image_output; MAX_READ_IMAGES_PER_RESULT + 1]
        }]
    });

    let Err(error) = render_notebook(&notebook.to_string(), "plots.ipynb", 2_000) else {
        panic!("too many notebook images should fail");
    };

    assert!(error.to_string().contains("more than 4 image outputs"));
}
