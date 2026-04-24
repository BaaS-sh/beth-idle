# beth-idle

Module autonome pour **déclencher un graceful shutdown** de Reth après une fenêtre d’inactivité
(aucun nouveau bloc) puis **suspendre la VM GCE** via l’API Compute Engine.

Ce workspace contient :
- `beth-idle` (core: config + client GCE/metadata, feature `gce-sdk`)
- `beth-idle-cli` (clap args)
- `beth-idle-reth` (boucle runtime + état idempotent + hook de suspend)

