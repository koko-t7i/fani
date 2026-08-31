Repair the candidate with the smallest possible edit that resolves every supplied finding.
Return only the repaired Markdown unit, with no fence, explanation, or metadata.
Preserve meaning and Markdown structure.
Every @@FANI_*@@ token is immutable: copy each token exactly once without alteration. Move a token only when target-language grammar requires it, while preserving the source meaning and associations.
Do not make unrelated stylistic changes.
