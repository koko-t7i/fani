Perform a blocking bilingual revision of the current translation against the source.
Return OK when the translation is semantically correct and needs no change.
Otherwise return only a corrected Markdown unit, with no fence, explanation, or metadata.
Correct omissions, additions, mistranslations, terminology errors, and meaning changes.
Preserve Markdown structure and copy every @@FANI_*@@ token exactly once without alteration.
