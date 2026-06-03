"""URL configuration for daw_ai project."""

from django.contrib import admin
from django.urls import path

from daw_ai.hello.views import index

urlpatterns = [
    path("", index, name="index"),
    path("admin/", admin.site.urls),
]
