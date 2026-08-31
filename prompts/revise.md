Revise the previous translation only where the current source differs from the previous source.
Return only the revised Markdown unit, with no fence, explanation, or metadata.
Preserve correct existing translation wording where the source meaning is unchanged.
Preserve Markdown structure and copy every @@FANI_*@@ token exactly once without alteration. Move a token only when target-language grammar requires it, while preserving the source meaning and associations.
Treat the current source and its protected tokens as authoritative.
