use crate::function_tool::FunctionCallError;
use crate::tools::handlers::file_path::summarize_tool_argument;
use crate::tools::handlers::file_state::normalize_file_content;

pub(super) const MAX_EDIT_FILE_SIZE: u64 = 512 * 1024 * 1024;

pub(super) struct EditContentRequest<'a> {
    pub old_string: &'a str,
    pub new_string: &'a str,
    pub replace_all: bool,
}

pub(super) struct PreparedEdit {
    pub updated_content: String,
}

pub(super) fn prepare_edit(
    request: EditContentRequest<'_>,
    original: Option<&str>,
) -> Result<PreparedEdit, FunctionCallError> {
    if request.new_string.len() > MAX_EDIT_FILE_SIZE as usize {
        return Err(FunctionCallError::RespondToModel(format!(
            "Replacement content is too large ({} bytes). Maximum editable file size is {MAX_EDIT_FILE_SIZE} bytes.",
            request.new_string.len()
        )));
    }
    let Some(original) = original else {
        if request.old_string.is_empty() {
            return Ok(PreparedEdit {
                updated_content: request.new_string.to_string(),
            });
        }
        return Err(FunctionCallError::RespondToModel(
            "File does not exist.".to_string(),
        ));
    };

    if request.old_string.is_empty() {
        if !normalize_file_content(original).trim().is_empty() {
            return Err(FunctionCallError::RespondToModel(
                "Cannot create new file - file already exists.".to_string(),
            ));
        }
        return Ok(PreparedEdit {
            updated_content: request.new_string.to_string(),
        });
    }

    let actual_old_string = find_actual_string(original, request.old_string).ok_or_else(|| {
        let old_string = summarize_tool_argument(request.old_string);
        FunctionCallError::RespondToModel(format!(
            "String to replace not found in file.\nString: {old_string}"
        ))
    })?;
    let matches = original.matches(&actual_old_string).count();
    if matches > 1 && !request.replace_all {
        let old_string = summarize_tool_argument(request.old_string);
        return Err(FunctionCallError::RespondToModel(format!(
            "Found {matches} matches of the string to replace, but replace_all is false. To replace all occurrences, set replace_all to true. To replace only one occurrence, please provide more context to uniquely identify the instance.\nString: {old_string}"
        )));
    }

    let actual_new_string = preserve_line_endings(
        request.old_string,
        &actual_old_string,
        &preserve_quote_style(request.old_string, &actual_old_string, request.new_string),
    );
    let (search, replacement) =
        if actual_new_string.is_empty() && !actual_old_string.ends_with('\n') {
            let line_ending = if original.contains(&format!("{actual_old_string}\r\n")) {
                "\r\n"
            } else {
                "\n"
            };
            let search_with_line_ending = format!("{actual_old_string}{line_ending}");
            if original.contains(&search_with_line_ending) {
                (search_with_line_ending, String::new())
            } else {
                (actual_old_string, actual_new_string)
            }
        } else {
            (actual_old_string, actual_new_string)
        };
    let updated_content = if request.replace_all {
        validate_updated_size(
            original.len(),
            search.len(),
            replacement.len(),
            original.matches(&search).count(),
        )?;
        original.replace(&search, &replacement)
    } else {
        validate_updated_size(original.len(), search.len(), replacement.len(), 1)?;
        original.replacen(&search, &replacement, 1)
    };
    if updated_content == original {
        return Err(FunctionCallError::RespondToModel(
            "No changes to make: the requested replacement leaves the file unchanged.".to_string(),
        ));
    }
    Ok(PreparedEdit { updated_content })
}

fn validate_updated_size(
    original_size: usize,
    search_size: usize,
    replacement_size: usize,
    replacement_count: usize,
) -> Result<(), FunctionCallError> {
    let removed_size = search_size.checked_mul(replacement_count);
    let added_size = replacement_size.checked_mul(replacement_count);
    let updated_size = removed_size
        .and_then(|removed_size| original_size.checked_sub(removed_size))
        .and_then(|remaining_size| {
            added_size.and_then(|added_size| remaining_size.checked_add(added_size))
        });
    if updated_size.is_none_or(|size| size > MAX_EDIT_FILE_SIZE as usize) {
        return Err(FunctionCallError::RespondToModel(format!(
            "The edited file would exceed the maximum editable file size of {MAX_EDIT_FILE_SIZE} bytes. Use smaller replacements or another editing strategy."
        )));
    }
    Ok(())
}

fn find_actual_string(file_content: &str, search_string: &str) -> Option<String> {
    if file_content.contains(search_string) {
        return Some(search_string.to_string());
    }

    let search_string = if file_content.contains("\r\n") {
        search_string.replace('\n', "\r\n")
    } else {
        search_string.to_string()
    };
    if file_content.contains(&search_string) {
        return Some(search_string);
    }

    let normalized_search = normalize_quotes(&search_string);
    let normalized_file = normalize_quotes(file_content);
    let start_byte = normalized_file.find(&normalized_search)?;
    let start_char = normalized_file[..start_byte].chars().count();
    let char_count = normalized_search.chars().count();
    Some(
        file_content
            .chars()
            .skip(start_char)
            .take(char_count)
            .collect(),
    )
}

fn preserve_line_endings(old_string: &str, actual_old_string: &str, new_string: &str) -> String {
    if !old_string.contains("\r\n") && actual_old_string.contains("\r\n") {
        new_string.replace('\n', "\r\n")
    } else {
        new_string.to_string()
    }
}

fn normalize_quotes(value: &str) -> String {
    value.replace(['‘', '’'], "'").replace(['“', '”'], "\"")
}

fn preserve_quote_style(old_string: &str, actual_old_string: &str, new_string: &str) -> String {
    if old_string == actual_old_string {
        return new_string.to_string();
    }

    let use_curly_double = actual_old_string.contains(['“', '”']);
    let use_curly_single = actual_old_string.contains(['‘', '’']);
    new_string
        .chars()
        .enumerate()
        .map(|(index, character)| match character {
            '\"' if use_curly_double && is_opening_quote(new_string, index) => '“',
            '\"' if use_curly_double => '”',
            '\'' if use_curly_single && is_opening_quote(new_string, index) => '‘',
            '\'' if use_curly_single => '’',
            _ => character,
        })
        .collect()
}

fn is_opening_quote(value: &str, char_index: usize) -> bool {
    if char_index == 0 {
        return true;
    }
    value.chars().nth(char_index - 1).is_some_and(|character| {
        character.is_whitespace() || matches!(character, '(' | '[' | '{' | '—' | '–')
    })
}

#[cfg(test)]
#[path = "file_edit_content_tests.rs"]
mod tests;
