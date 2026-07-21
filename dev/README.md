# Local dev servers

Throwaway local servers for exercising each backend. See the per-protocol README for
credentials and setup: [`irc/`](irc/README.md), [`matrix/`](matrix/README.md),
[`mattermost/`](mattermost/README.md).

Start them all at once from the repo root:

```bash
docker compose up -d
```

Or start just one, e.g. `docker compose -f dev/irc/docker-compose.yml up -d`.

## Pointing tirc at them

`require('tirc.dev')` (bundled, no filesystem access needed) has a ready-made
`TircConfigServer` entry for each dev server, using the credentials from the READMEs
above:

```lua
local dev = require('tirc.dev')
local utils = require('tirc.utils')

config.servers = utils.list_concat(dev.servers, my_real_servers)
```

Or pick individual entries (`dev.irc`, `dev.matrix`, `dev.matrix_sliding`,
`dev.mattermost`) to combine only some of them with your own servers.
