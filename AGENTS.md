# zchat development

Read README.md and docs/architecture.md before changing an interface. Keep all UI, CLI and
MCP operations on the shared Engine path. Preserve native provider payloads and report the
actual supported subset in each capability manifest.

Use canonical latest-default-branch Git dependencies for cross-repository Project Z code.
Do not add registry versions, tags or fixed revs to those development declarations. Registry
publication goes through scripts/release.py and its independently validated staging area.

Validate changes locally. Keep ordinary pushes free of routine CI triggers; formal release
workflows build native artifacts. Tests must use local fixtures and isolated account config.
Never use a real contact, paid account operation or real platform mutation as an implicit
test. Preserve the actual encryption and authentication boundary of each provider.

Generated binaries, screenshots, packages and runtime configuration belong in ignored
artifact directories or release assets. Source SVG icons and native test fixtures may be
tracked. Keep the complete runtime feature statement aligned with working code and test
evidence; an operation name in the generic contract is not a provider implementation.
