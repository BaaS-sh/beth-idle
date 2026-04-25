# beth-idle

Module autonome pour **suspendre une VM GCE à chaud** après une fenêtre d’inactivité
(aucun nouveau bloc), sans arrêter Beth/Reth.

Ce workspace contient :
- `beth-idle` (core: config + client GCE/metadata avec le SDK officiel Compute Engine, feature `gce-sdk`)
- `beth-idle-cli` (clap args)
- `beth-idle-reth` (boucle runtime idle -> suspend GCE -> reprise du cycle)

Au déclenchement idle, la boucle appelle `instances.suspend` avec `discardLocalSsd=false`
et attend l’opération GCP. La VM copie la RAM et l’état applicatif; au prochain resume,
le même process Beth reprend et le monitor redémarre un nouveau cycle idle.

Le module ne bloque pas la txpool/RPC pendant la fenêtre API -> suspend réel: les clients
doivent reconnecter/réessayer si la VM est déjà suspendue.
