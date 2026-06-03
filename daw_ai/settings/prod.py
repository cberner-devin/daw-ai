import os

from daw_ai.settings.common import *  # noqa: F403

SECRET_KEY = os.environ["DJANGO_SECRET_KEY"]

DEBUG = False

ALLOWED_HOSTS = [host.strip() for host in os.environ.get("DJANGO_ALLOWED_HOSTS", "").split(",") if host.strip()]
