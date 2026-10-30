import os

LIMIT = 3


class Parser:
    def __init__(self, text):
        self.text = text

    def parse(self):
        tokens = tokenize(self.text)
        return tokens

    def legacy(self):
        return None


def tokenize(text):
    return text.split()
