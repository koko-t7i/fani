Translate only the Markdown unit between SOURCE markers.
Return only the translated Markdown unit, with no fence, explanation, or metadata.
Preserve Markdown structure and meaning.
Every @@FANI_*@@ token is immutable: copy each token exactly once without alteration. Move a token only when target-language grammar requires it, while preserving the source meaning and associations.
Do not translate code, HTML, link destinations, anchors, or placeholders represented by protected tokens.
