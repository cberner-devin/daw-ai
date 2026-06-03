# daw-ai

A small Django project scaffolded from Hitch's CI, coding-agent, and local-development boilerplate.

## Local Development

Prerequisites:

- Python 3.13 or newer
- `uv`
- `just`

Run the development server:

```sh
just run
```

Pass a port when `8000` is already in use:

```sh
just run 8001
```

Run the full local check suite:

```sh
just test
```

The root URL serves a minimal `Hello world` response from the `daw_ai.hello` app.
