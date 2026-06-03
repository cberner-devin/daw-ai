"""WSGI config for daw_ai project."""

import os

from django.core.wsgi import get_wsgi_application

os.environ.setdefault("DJANGO_SETTINGS_MODULE", "daw_ai.settings.dev")

application = get_wsgi_application()
