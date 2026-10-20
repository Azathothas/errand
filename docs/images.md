# Generating images

A provider whose endpoint answers the OpenAI Images API
(`POST /images/generations`) can give sessions an image tool. Mark it with
`images`, naming the model a call asks for when it names none and, if the agent
may choose, the models it may choose from:

```json
{
  "agent": {
    "providers": {
      "gateway": {
        "baseUrl": "https://gateway.example/v1",
        "credential": "...",
        "models": [{ "id": "chat-model" }],
        "images": { "model": "image-small", "models": ["image-small", "image-large"] }
      }
    }
  }
}
```

A provider the agent already uses for chat lends the tool its endpoint and its
key, as above, and so does one kage knows by name, such as `openai`. A provider
kage does not know, which serves images and no chat model, is described without
`models`:

```json
"pictures": {
  "baseUrl": "https://images.example/v1",
  "credential": "...",
  "images": { "model": "image-small" }
}
```

It is written for the image tool alone, so `!model` never offers its image
models, and it needs a `baseUrl`: without one there is nowhere to send a call,
and sessions run without the tool. With `sandbox.egress.mode` set to `proxy` it
is brokered like any other provider, so the sandbox holds a nonce in place of
its key.

Only one provider may carry `images`. An `anthropic` provider cannot, since that
API has no images endpoint, and neither can kage's own `openai-responses`
provider, or one a plugin registers.

The tool needs a kage that has it. An older kage ignores the setting, and its
sessions run without the tool.
