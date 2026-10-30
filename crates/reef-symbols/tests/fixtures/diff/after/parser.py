import os
import sys

LIMIT = 3


class Parser:
    def __init__(self, text):
        self.text = text

    def parse(self):
        tokens = tokenize(self.text)
        return [t for t in tokens if t]


def tokenize(text):
    return text.split()


def helper():
    return sys.argv
