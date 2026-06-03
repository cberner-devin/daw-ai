run port="8000": pre
  uv run python ./manage.py migrate --settings daw_ai.settings.dev
  uv run python ./manage.py runserver 127.0.0.1:{{port}} --settings daw_ai.settings.dev

pre: sync
  uv run ruff check .
  uv run mypy .

test: pre
  uv run python -Wa ./manage.py test --settings daw_ai.settings.dev

coverage: pre
  uv run coverage run ./manage.py test --settings daw_ai.settings.dev
  uv run coverage report
  uv run coverage xml
  uv run coverage html

format:
  uv run ruff format .
  uv run ruff check --select I --fix

sync:
  uv sync --all-groups
