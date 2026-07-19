use super::*;
use pretty_assertions::assert_eq;

#[test]
fn device_path_filter_blocks_infinite_and_stdio_sources() {
    assert!(is_blocked_device_path("/dev/zero"));
    assert!(is_blocked_device_path("/proc/self/fd/0"));
    assert!(is_blocked_device_path("/proc/42/fd/2"));
    assert!(!is_blocked_device_path("/dev/null"));
    assert!(!is_blocked_device_path("/tmp/fd/0"));
}

#[test]
fn image_prompt_size_is_hard_bounded() {
    assert_eq!(
        validate_image_prompt_size(MAX_READ_IMAGE_BYTES, "Image"),
        Ok(())
    );
    assert_eq!(
        validate_image_prompt_size(MAX_READ_IMAGE_BYTES + 1, "Image"),
        Err(FunctionCallError::RespondToModel(format!(
            "Image content ({} bytes) exceeds the maximum safe prompt size ({MAX_READ_IMAGE_BYTES} bytes). Resize or compress the image before reading it.",
            MAX_READ_IMAGE_BYTES + 1
        )))
    );
}
