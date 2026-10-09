# Environment variables

| Variable        | Effect                                                                                 |
| --------------- | -------------------------------------------------------------------------------------- |
| `KONFORM_COLOR` | Sets the default for `--color` (`auto`, `always`, `never`). The flag takes precedence. |
| `NO_COLOR`      | When set, disables colour in `auto` mode ([no-color.org](https://no-color.org)).       |
| `TERM`          | `TERM=dumb` disables colour in `auto` mode.                                            |

With `--color auto` (the default), colour is emitted only when stderr is a TTY,
`NO_COLOR` is unset and `TERM` is not `dumb`.
