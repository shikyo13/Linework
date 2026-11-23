"""Linework's command and event registration for Sublime Text packages."""

import re


class Command:
    @classmethod
    def name(cls):
        name = cls.__name__.removesuffix("Command")
        return re.sub(
            r"(?<=[a-z0-9])(?=[A-Z])|(?<=[A-Z])(?=[A-Z][a-z])", "_", name
        ).lower()

    def is_enabled(self, **args):
        return True

    def is_visible(self, **args):
        return True

    def description(self, **args):
        return self.name().replace("_", " ").title()


class TextCommand(Command):
    def __init__(self, view):
        self.view = view


class WindowCommand(Command):
    def __init__(self, window):
        self.window = window


class ApplicationCommand(Command):
    pass


class EventListener:
    pass


class ViewEventListener:
    def __init__(self, view):
        self.view = view

    @classmethod
    def is_applicable(cls, settings):
        return True
