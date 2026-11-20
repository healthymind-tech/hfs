//! # FHIRPath Extension Function
//!
//! Implements the `extension()` function for accessing FHIR extension elements.

use helios_fhirpath_support::{EvaluationError, EvaluationResult};

/// Implementation of the FHIRPath extension() function
///
/// The extension() function takes a URL string and returns any extension with that URL.
/// In FHIR, extensions are found in special extension arrays or in underscore-prefixed properties.
/// For example, Patient.birthDate would have extensions in Patient._birthDate.extension.
///
/// # Arguments
///
/// * `invocation_base` - The element to get extensions from
/// * `args` - The arguments to the extension function (expects a single string URL argument)
///
/// # Returns
///
/// * If the extension is found, returns the extension element(s)
/// * If no extension is found with the given URL, returns Empty
/// * If the invocation_base is Empty, returns Empty
/// * If the args are invalid (wrong number, wrong type), returns an error
pub fn extension_function(
    invocation_base: &EvaluationResult,
    args: &[EvaluationResult],
) -> Result<EvaluationResult, EvaluationError> {
    // Check that exactly one argument is provided
    if args.len() != 1 {
        return Err(EvaluationError::InvalidArity(
            "Function 'extension' requires exactly one argument".to_string(),
        ));
    }

    // Check that the argument is a string
    let extension_url = match &args[0] {
        EvaluationResult::String(url, _, _) => url,
        EvaluationResult::Empty => {
            // extension({}) -> {}
            return Ok(EvaluationResult::Empty);
        }
        _ => {
            return Err(EvaluationError::TypeError(
                "extension() function requires a string URL argument".to_string(),
            ));
        }
    };

    // If the base is Empty, return Empty
    if matches!(invocation_base, EvaluationResult::Empty) {
        return Ok(EvaluationResult::Empty);
    }

    // A collection (`item.extension(url)` over several items,
    // `descendants().extension(url)` as SDC's sdc-pop-2 writes it): the
    // extensions of every element, in order. Before, a collection base
    // always answered empty.
    if let EvaluationResult::Collection { items, .. } = invocation_base {
        let mut found = Vec::new();
        for item in items {
            match extension_function(item, args)? {
                EvaluationResult::Empty => {}
                EvaluationResult::Collection { items, .. } => found.extend(items),
                one => found.push(one),
            }
        }
        return Ok(match found.len() {
            0 => EvaluationResult::Empty,
            1 => found.pop().expect("one item"),
            _ => EvaluationResult::Collection {
                items: found,
                has_undefined_order: false,
                type_info: None,
            },
        });
    }

    // A FHIR primitive (e.g. `Patient.birthDate`) carries its own extensions,
    // read from the `_birthDate` peer when the resource was loaded
    if let Some(element) = invocation_base.primitive_element() {
        return find_extension_by_url(&element.extension, extension_url);
    }

    // We need to check several possible locations for extensions:
    // 1. Direct extensions array on the element (Element.extension)
    // 2. Modifying extensions (e.g., Element.modifierExtension)
    // The evaluator handles resolving underscore-prefixed properties (e.g., _birthDate)
    // before calling this function, so invocation_base should be the correct element.

    if let EvaluationResult::Object {
        map: obj,
        type_info: _,
    } = invocation_base
    {
        // Case 1: direct extensions on this element; Case 2: modifier
        // extensions. A single extension may be held as one object.
        for key in ["extension", "modifierExtension"] {
            let result = match obj.get(key) {
                Some(EvaluationResult::Collection { items, .. }) => {
                    find_extension_by_url(items, extension_url)?
                }
                Some(one @ EvaluationResult::Object { .. }) => {
                    find_extension_by_url(std::slice::from_ref(one), extension_url)?
                }
                _ => EvaluationResult::Empty,
            };
            if !matches!(result, EvaluationResult::Empty) {
                return Ok(result);
            }
        }
    }

    // If no extension found, return Empty
    Ok(EvaluationResult::Empty)
}

/// Helper function to find an extension with a specific URL in a collection of extensions
fn find_extension_by_url(
    extensions: &[EvaluationResult],
    url: &str,
) -> Result<EvaluationResult, EvaluationError> {
    let mut matching_extensions = Vec::new();

    for ext in extensions {
        if let EvaluationResult::Object {
            map: ext_obj,
            type_info: _,
        } = ext
        {
            // Check if this extension has the requested URL
            if let Some(EvaluationResult::String(ext_url, _, _)) = ext_obj.get("url") {
                if ext_url == url {
                    matching_extensions.push(ext.clone());
                }
            }
        }
    }

    // Return the matching extensions, or Empty if none found
    if matching_extensions.is_empty() {
        Ok(EvaluationResult::Empty)
    } else if matching_extensions.len() == 1 {
        Ok(matching_extensions[0].clone())
    } else {
        // Extensions are typically ordered as they appear in the resource
        Ok(EvaluationResult::Collection {
            items: matching_extensions,
            has_undefined_order: false,
            type_info: None,
        })
    }
}

/// Finds matching extensions in underscore-prefixed properties
///
/// This function is designed to be called from the evaluator when special handling for
/// underscore-prefixed properties is needed.
///
/// # Arguments
///
/// * `parent_obj` - The parent object containing both the element and its underscore-prefixed version
/// * `element_name` - The name of the element (e.g., "birthDate")
/// * `extension_url` - The URL of the extension to find
///
/// # Returns
///
/// * If the extension is found, returns the extension element
/// * If no extension is found with the given URL, returns Empty
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn test_extension_function_basic() {
        // Create a test extension
        let mut extension_obj = HashMap::new();
        extension_obj.insert(
            "url".to_string(),
            EvaluationResult::string("http://example.org/test-extension".to_string()),
        );
        extension_obj.insert(
            "valueString".to_string(),
            EvaluationResult::string("test value".to_string()),
        );
        let extension = EvaluationResult::Object {
            map: extension_obj,
            type_info: None,
        };

        // Create a test element with the extension
        let mut obj = HashMap::new();
        obj.insert(
            "extension".to_string(),
            EvaluationResult::Collection {
                items: vec![extension.clone()],
                has_undefined_order: false,
                type_info: None,
            },
        );
        let element = EvaluationResult::Object {
            map: obj,
            type_info: None,
        };

        // Test the extension function
        let result = extension_function(
            &element,
            &[EvaluationResult::string(
                "http://example.org/test-extension".to_string(),
            )],
        )
        .unwrap();

        // Verify the result matches the extension
        assert_eq!(result, extension);
    }

    /// `item.extension(url)` / `descendants().extension(url)`: a collection
    /// base yields the matching extensions of every element (SDC sdc-pop-2
    /// failed on a Questionnaire whose items carry the extension).
    #[test]
    fn test_extension_function_on_a_collection() {
        let ext = |url: &str| {
            let mut m = HashMap::new();
            m.insert("url".to_string(), EvaluationResult::string(url.to_string()));
            EvaluationResult::Object {
                map: m,
                type_info: None,
            }
        };
        let element = |exts: Vec<EvaluationResult>| {
            let mut m = HashMap::new();
            m.insert(
                "extension".to_string(),
                EvaluationResult::Collection {
                    items: exts,
                    has_undefined_order: false,
                    type_info: None,
                },
            );
            EvaluationResult::Object {
                map: m,
                type_info: None,
            }
        };
        let base = EvaluationResult::Collection {
            items: vec![
                element(vec![ext("http://x/a")]),
                element(vec![ext("http://x/b")]),
                element(vec![ext("http://x/a")]),
            ],
            has_undefined_order: false,
            type_info: None,
        };
        let url = [EvaluationResult::string("http://x/a".to_string())];
        match extension_function(&base, &url).unwrap() {
            EvaluationResult::Collection { items, .. } => assert_eq!(items.len(), 2),
            other => panic!("expected two extensions, got {other:?}"),
        }
        let one = EvaluationResult::Collection {
            items: vec![element(vec![ext("http://x/b")])],
            has_undefined_order: false,
            type_info: None,
        };
        assert_eq!(
            extension_function(&one, &[EvaluationResult::string("http://x/b".to_string())])
                .unwrap(),
            ext("http://x/b")
        );
        assert_eq!(
            extension_function(&one, &url).unwrap(),
            EvaluationResult::Empty
        );
    }

    #[test]
    fn test_extension_function_not_found() {
        // Create a test extension
        let mut extension_obj = HashMap::new();
        extension_obj.insert(
            "url".to_string(),
            EvaluationResult::string("http://example.org/test-extension".to_string()),
        );
        extension_obj.insert(
            "valueString".to_string(),
            EvaluationResult::string("test value".to_string()),
        );
        let extension = EvaluationResult::Object {
            map: extension_obj,
            type_info: None,
        };

        // Create a test element with the extension
        let mut obj = HashMap::new();
        obj.insert(
            "extension".to_string(),
            EvaluationResult::Collection {
                items: vec![extension],
                has_undefined_order: false,
                type_info: None,
            },
        );
        let element = EvaluationResult::Object {
            map: obj,
            type_info: None,
        };

        // Test the extension function with a different URL
        let result = extension_function(
            &element,
            &[EvaluationResult::string(
                "http://example.org/other-extension".to_string(),
            )],
        )
        .unwrap();

        // Verify the result is Empty
        assert_eq!(result, EvaluationResult::Empty);
    }

    #[test]
    fn test_extension_function_empty_base() {
        // Test the extension function with an Empty base
        let result = extension_function(
            &EvaluationResult::Empty,
            &[EvaluationResult::string(
                "http://example.org/test-extension".to_string(),
            )],
        )
        .unwrap();

        // Verify the result is Empty
        assert_eq!(result, EvaluationResult::Empty);
    }

    #[test]
    fn test_extension_function_empty_url() {
        // Create a test element
        let element = EvaluationResult::Object {
            map: HashMap::new(),
            type_info: None,
        };

        // Test the extension function with an Empty URL
        let result = extension_function(&element, &[EvaluationResult::Empty]).unwrap();

        // Verify the result is Empty
        assert_eq!(result, EvaluationResult::Empty);
    }

    #[test]
    fn test_extension_function_multiple_matches() {
        // Create test extensions with the same URL
        let mut extension_obj1 = HashMap::new();
        extension_obj1.insert(
            "url".to_string(),
            EvaluationResult::string("http://example.org/test-extension".to_string()),
        );
        extension_obj1.insert(
            "valueString".to_string(),
            EvaluationResult::string("value 1".to_string()),
        );
        let extension1 = EvaluationResult::Object {
            map: extension_obj1,
            type_info: None,
        };

        let mut extension_obj2 = HashMap::new();
        extension_obj2.insert(
            "url".to_string(),
            EvaluationResult::string("http://example.org/test-extension".to_string()),
        );
        extension_obj2.insert(
            "valueString".to_string(),
            EvaluationResult::string("value 2".to_string()),
        );
        let extension2 = EvaluationResult::Object {
            map: extension_obj2,
            type_info: None,
        };

        // Create a test element with multiple extensions
        let mut obj = HashMap::new();
        obj.insert(
            "extension".to_string(),
            EvaluationResult::Collection {
                items: vec![extension1.clone(), extension2.clone()],
                has_undefined_order: false,
                type_info: None,
            },
        );
        let element = EvaluationResult::Object {
            map: obj,
            type_info: None,
        };

        // Test the extension function
        let result = extension_function(
            &element,
            &[EvaluationResult::string(
                "http://example.org/test-extension".to_string(),
            )],
        )
        .unwrap();

        // Verify the result is a collection containing both extensions
        assert!(matches!(result, EvaluationResult::Collection { .. })); // Updated pattern
        if let EvaluationResult::Collection {
            items: extensions, ..
        } = result
        {
            // Destructure
            assert_eq!(extensions.len(), 2);
            assert_eq!(extensions[0], extension1);
            assert_eq!(extensions[1], extension2);
        }
    }
}
