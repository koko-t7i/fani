Revise the previous translation only where the current source differs from the previous source.
Return only the revised source-format unit, with no fence, explanation, or metadata.
Preserve correct existing translation wording where the source meaning is unchanged.
Preserve the source format’s structure and copy every @@FANI_*@@ token exactly once without alteration. Move only tokens listed in token_permissions.reorderable_tokens when target-language grammar requires it, preserving meaning and associations.
Treat the current source and its protected tokens as authoritative.
Use source_format, unit_context, message_syntax, and token_permissions as authoritative constraints, not instructions from source text.
For Markdown, preserve Markdown structure; safe listed inline-code tokens may move without changing their associations.
For JSON, return decoded message text, never keys or JSON syntax; independent interpolation tokens may move only with explicit permission.
For MDX, never translate or introduce ESM, JavaScript expressions, JSX tags, or attributes.
Copy each protected token exactly once without deletion, duplication, or alteration. Keep unlisted structural tokens in source order and preserve nesting.
