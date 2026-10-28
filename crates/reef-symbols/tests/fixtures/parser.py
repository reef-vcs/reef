import functools

MAX_DEPTH = 8
default_name = "x"


class Parser(Base, mixins.Logged):
    def __init__(self, text):
        self.text = text

    @functools.cache
    def parse(self):
        tokens = tokenize(self.text)
        return self.build(tokens)

    def build(self, tokens):
        def step(token):
            return emit(token)

        return [step(t) for t in tokens]


@register("parsers")
def make_parser(text):
    return Parser(text)
