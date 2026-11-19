# Override configuration from environment variables

In containers we cannot edit the JSON file, so every setting must also be settable from an environment variable.
Add environment variables as a third source, after the defaults and the file, so a variable wins over both.

* `loadConfig` takes the variables as an object, typically `process.env`. Without one, no variable is read (as today).
* Only variables that start with a prefix are read (see Interface for the default and the naming). The rest of the
  name is the key: lower case, with a double underscore between levels. A variable with the prefix whose key is not in
  the schema is an error.
* A variable is text, read for the field's type: a `number` is a decimal such as `80` or `-1.5` (anything else is an
  error, `80px` and the empty string included); a `boolean` is `true`, `false`, `1` or `0`; a `list` is comma-separated,
  each item trimmed and empty items dropped (so the empty string is an empty list); an `enum` must be one of its values;
  a `string` is taken as it is.
* Every error is a `ConfigError` that names the variable where the file errors name the file, and uses the same
  wording as the file's type errors (examples in Interface).

Document it in the README. The existing tests must keep passing (`npm test`).

## Interface

* `loadConfig({ schema, file, optional, env, envPrefix })`: `env` is the object of variables (default: none),
  `envPrefix` the prefix (default `'APP_'`).
* Names: `APP_SERVER__PORT` is the key `server.port`, `APP_LOG__MAX_FILES` is `log.max_files`.
* Messages: `APP_SERVER__PORT: server.port must be a number`, `APP_DEBUG: debug must be true or false`,
  `APP_SERVER__COLOUR: unknown key server.colour`.
