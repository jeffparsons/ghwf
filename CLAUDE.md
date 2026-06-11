# ghwf development notes

## Adding a config option

Give the new field a `///` doc comment — that's the single source the
`ghwf config ls`/`info`/`example` commands surface via facet reflection. Then
also:

- offer it in the `ghwf config init` wizard (`src/init.rs`) so interactive
  setup stays complete,
- document it in the README's annotated `ghwf.toml` example, and
- add it to `ghwf config example` (`src/config_schema.rs`): the
  `example_covers_every_field` guard won't compile until the new field is
  destructured there, and the `example_*` tests fail until it's actually
  emitted with a value.
