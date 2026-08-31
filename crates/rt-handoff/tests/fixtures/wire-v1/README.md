# Golden corpus — wire protocol v1

Committed payloads produced by the first build that shipped the format. Every
future version of `rt-handoff` must decode all of them, and must re-encode each
one to byte-identical output.

**Do not regenerate these to make a test pass.** The test failing is the
corpus doing its job: it means the format changed, which means a newer rt can
no longer read an older rt's panes. Either revert the change or add a new tag
instead of altering an existing one (rule R1).

Adding fixtures is fine and welcome. Changing or deleting one is a protocol
break and needs the same scrutiny as changing the spec.
