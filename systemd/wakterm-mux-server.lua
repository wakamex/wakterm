local wakterm = require 'wakterm'

local config = wakterm.config_builder()

config.unix_domains = {
  {
    name = 'unix',
    socket_path = '/run/wakterm/sock',
  },
}

return config
