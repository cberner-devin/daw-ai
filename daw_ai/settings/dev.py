import os

from daw_ai.settings.common import *  # noqa: F403

# Quick-start development settings - unsuitable for production.

_DEV_SECRET_KEY = "django-insecure-7eoa98prb_o7=s9f_k4s4j@jl!2u8$1my+f0=v6mc+g0csjj5t"
SECRET_KEY = os.environ.get("DJANGO_SECRET_KEY", _DEV_SECRET_KEY)

DEBUG = True

ALLOWED_HOSTS = ["localhost", "127.0.0.1"]
ALLOWED_HOSTS += [host.strip() for host in os.environ.get("ADDITIONAL_ALLOWED_HOSTS", "").split(",") if host.strip()]
INTERNAL_IPS = ["localhost", "127.0.0.1"]

CSRF_TRUSTED_ORIGINS = [
    f"{scheme}://{'*' + host if host.startswith('.') else host}"
    for host in ALLOWED_HOSTS
    if host != "*"
    for scheme in ("http", "https")
]
