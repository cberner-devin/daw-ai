"""ASGI config for daw_ai project."""

import os

from django.core.asgi import get_asgi_application

os.environ.setdefault("DJANGO_SETTINGS_MODULE", "daw_ai.settings.dev")

application = get_asgi_application()
