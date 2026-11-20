#!/usr/bin/env python3
"""Pack OpenAI Whisper's multilingual.tiktoken into crates/local-whisper/assets/multilingual.vocab.

The source lists one `base64(token bytes) rank` pair per line, ranks 0..50256 in order. The packed
form is the same tokens in rank order, each as one length byte followed by its bytes — a third of
the size, and read with no base64 or line parsing at startup. Special tokens are not in the file:
every Whisper tokenizer appends them after the last rank, so local-whisper derives their ids.

  vocab.py <multilingual.tiktoken> <out.vocab>
"""
import base64
import sys

src, dst = sys.argv[1], sys.argv[2]
tokens = []
for line in open(src, "rb"):
    parts = line.split()
    if len(parts) != 2:
        continue
    assert int(parts[1]) == len(tokens), "ranks must be dense and ordered"
    tokens.append(base64.b64decode(parts[0]))
assert len(tokens) == 50257, len(tokens)
assert max(map(len, tokens)) <= 255
with open(dst, "wb") as out:
    for token in tokens:
        out.write(bytes([len(token)]))
        out.write(token)
print(f"{len(tokens)} tokens, longest {max(map(len, tokens))} bytes -> {dst}")
