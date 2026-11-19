//! # FHIRPath RepeatAll Function
//!
//! Implements the `repeatAll()` function for recursive traversal that preserves duplicates.

use crate::evaluator::{EvaluationContext, evaluate};
use crate::parser::Expression;
use helios_fhirpath_support::{EvaluationError, EvaluationResult};

/// Maximum number of items to prevent infinite loops on cyclic data.
const MAX_ITEMS: usize = 10_000;

/// Implements the FHIRPath repeatAll() function
///
/// Like `repeat()`, but does NOT deduplicate items. Duplicates are preserved.
/// Stops when an iteration produces no new items (fixed point).
/// Has a safety limit to prevent infinite loops on cyclic data.
///
/// # Syntax
/// `repeatAll(projection: expression) : collection`
pub fn repeat_all_function(
    invocation_base: &EvaluationResult,
    projection_expr: &Expression,
    context: &EvaluationContext,
) -> Result<EvaluationResult, EvaluationError> {
    let initial_items = match invocation_base {
        EvaluationResult::Collection { items, .. } => items.clone(),
        EvaluationResult::Empty => Vec::new(),
        single_item => vec![single_item.clone()],
    };

    let mut result = Vec::new();
    let mut items_to_process = initial_items;

    while !items_to_process.is_empty() {
        let mut new_items_to_process = Vec::new();

        for item in &items_to_process {
            let projected = evaluate(projection_expr, context, Some(item))?;

            let projected_items = match projected {
                EvaluationResult::Collection { items, .. } => items,
                EvaluationResult::Empty => Vec::new(),
                single_item => vec![single_item],
            };

            for projected_item in projected_items {
                result.push(projected_item.clone());
                new_items_to_process.push(projected_item);

                if result.len() >= MAX_ITEMS {
                    return Err(EvaluationError::InvalidArgument(format!(
                        "repeatAll() exceeded safety limit of {} items",
                        MAX_ITEMS
                    )));
                }
            }
        }

        items_to_process = new_items_to_process;
    }

    if result.is_empty() {
        Ok(EvaluationResult::Empty)
    } else if result.len() == 1 {
        Ok(result.into_iter().next().unwrap())
    } else {
        Ok(EvaluationResult::Collection {
            items: result,
            has_undefined_order: true,
            type_info: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parser;
    use chumsky::Parser;

    #[test]
    fn test_repeat_all_empty() {
        let empty = EvaluationResult::Empty;
        let context = EvaluationContext::new_empty_with_default_version();
        let parsed = parser().parse("name").into_result().unwrap();
        let result = repeat_all_function(&empty, &parsed, &context).unwrap();
        assert_eq!(result, EvaluationResult::Empty);
    }

    #[test]
    fn test_repeat_all_basic() {
        // Create a simple hierarchy: root -> child -> grandchild
        let grandchild = EvaluationResult::Object {
            map: {
                let mut map = std::collections::HashMap::new();
                map.insert(
                    "name".to_string(),
                    EvaluationResult::string("level2".to_string()),
                );
                map
            },
            type_info: None,
        };

        let child = EvaluationResult::Object {
            map: {
                let mut map = std::collections::HashMap::new();
                map.insert(
                    "name".to_string(),
                    EvaluationResult::string("level1".to_string()),
                );
                map.insert("child".to_string(), grandchild);
                map
            },
            type_info: None,
        };

        let root = EvaluationResult::Object {
            map: {
                let mut map = std::collections::HashMap::new();
                map.insert(
                    "name".to_string(),
                    EvaluationResult::string("root".to_string()),
                );
                map.insert("child".to_string(), child);
                map
            },
            type_info: None,
        };

        let mut context = EvaluationContext::new_empty_with_default_version();
        context.this = Some(std::sync::Arc::new(root.clone()));

        let parsed = parser().parse("child").into_result().unwrap();
        let result = repeat_all_function(&root, &parsed, &context).unwrap();

        // Should have level1 and level2 objects
        match result {
            EvaluationResult::Collection { items, .. } => {
                assert_eq!(items.len(), 2);
            }
            _ => panic!("Expected collection result"),
        }
    }
}
