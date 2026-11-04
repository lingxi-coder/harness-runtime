# Teams identity and collaboration

Teams is a product service backed by its authenticated account and team
membership. Use native Teams UI and its current connector status for membership,
messages and Bot participants. The TeamsService owns listeners, cache and
account/epoch changes; signing into a different account must not reuse another
account's cached team data.

A team member, a personal Bot, a same-account remote viewer, an App learner and
a provider account have different identities and grants. Team membership does
not approve a Remote Control device, reveal a host's files or grant a provider
credential. A Teams Bot participates through its configured Bot/connector;
do not promise a cloud-executed Bot from a team listing.

The guide has no universal Teams management model tool. Use exposed native or
connector schemas, and distinguish saved membership, listener readiness,
delivery, and a Bot task's actual completion. Do not invent HTTP endpoints.

Source anchors: product apps/electron/src/main/teams/{service,connector}.ts,
docs/TEAMS.md, and the host's TeamsService/TeamConnector composition.
