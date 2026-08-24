import json

# https://mkdocs-macros-plugin.readthedocs.io/en/latest/macros/
def define_env(env):
    with open("docs/releases.json") as f:
        for (k, v) in json.load(f).items():
            env.variables[k] = v
