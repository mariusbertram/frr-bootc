# frr-bootc

Ein [bootc](https://containers.github.io/bootc/)-Image, das [FRR](https://frrouting.org/)
als Router-Appliance betreibt und dafür gedacht ist, als VM unter
**OpenShift Virtualization (KubeVirt)** zu laufen.

FRR- und Netzwerk-Konfiguration werden **nicht** ins Image gebacken, sondern
zur Laufzeit aus zwei Kubernetes-`ConfigMaps` in die VM gemountet und dort
von systemd-Services überwacht und automatisch angewendet.

## Architektur

```
                         OpenShift / KubeVirt
                         ─────────────────────
 ConfigMap "frr-config"      ConfigMap "network-config"
        │                            │
        │ virtiofs                  │ virtiofs
        ▼                            ▼
 /run/config/frr (ro)        /run/config/network (ro)
        │                            │
        ▼                            ▼
 frr-config-sync.path        network-config-sync.path
 (inotify auf Verzeichnis)   (inotify auf Verzeichnis)
        │                            │
        ▼                            ▼
 frr-config-sync.service     frr-bootc-ifnaming.service (MAC → Name)
   → /etc/frr/*               network-config-sync.service
   → vtysh -C (Validierung)     → *.nmconnection nach NetworkManager
   → frr-reload.py (live)       → nmstatectl apply *.yml/*.yaml
   → bei daemons-Änderung:
     systemctl restart frr
```

Beide Config-Quellen werden per **virtiofs** (nicht als Disk-Image) in die
VM gemountet. Das ist der von KubeVirt vorgesehene Weg, um `ConfigMap`-,
`Secret`- und `ServiceAccount`-Inhalte 1:1 als Dateien in eine VM zu geben,
ohne ein Cloud-Init-ISO oder einen Reboot zu benötigen.

### Warum zwei getrennte Sync-Pfade?

- **FRR-Konfiguration** (`frr.conf`, `daemons`, `vtysh.conf`) wird bei jeder
  Änderung neu synchronisiert. `frr.conf`-Änderungen werden per
  `vtysh -C` validiert und dann live über `frr-reload.py` übernommen (kein
  Neustart, keine Unterbrechung laufender Sessions/Nachbarschaften, soweit
  FRR das zulässt). Ändert sich `daemons` (z. B. `bgpd` wird neu aktiviert),
  ist ein Neustart von `frr.service` unvermeidlich, da das die laufenden
  Daemon-Prozesse bestimmt.
- **Netzwerkkonfiguration** wird in zwei Schritten angewendet:
  1. `frr-bootc-ifnaming.service` läuft **vor** `NetworkManager.service`
     und schreibt aus `interfaces.yaml` `.link`-Dateien
     (`/etc/systemd/network/70-frr-bootc-<name>.link`), die jedes Interface
     anhand seiner MAC-Adresse fest auf einen Namen pinnen.
  2. `network-config-sync.service` läuft **nach** `NetworkManager.service`
     und wendet die eigentliche Konfiguration an (nmstate oder
     NetworkManager-Keyfiles).

  Diese Trennung ist nötig, weil Interface-Umbenennung vor dem Start von
  NetworkManager passieren muss, `nmstatectl` aber einen laufenden
  NetworkManager voraussetzt.

## Konfigurationsformat

### `frr-config` ConfigMap → `/run/config/frr`

Beliebige Dateien werden 1:1 nach `/etc/frr/` gespiegelt. Relevant sind
insbesondere:

- `daemons` — welche FRR-Daemons laufen (siehe FRR-Doku)
- `frr.conf` — die eigentliche Routing-Konfiguration
- `vtysh.conf` — optional

Siehe [`manifests/10-configmap-frr-config.yaml`](manifests/10-configmap-frr-config.yaml).

### `network-config` ConfigMap → `/run/config/network`

- `interfaces.yaml` (optional, aber empfohlen) — MAC-Adresse-zu-Name-Mapping,
  nur für Interfaces, die KubeVirt der VM tatsächlich als PCI/virtio-Gerät
  präsentiert (also den physischen Uplink/Trunk und das LAN-Interface, nicht
  die per nmstate erzeugten VLAN-Sub-Interfaces der einzelnen Tenants):

  ```yaml
  interfaces:
    - mac: "02:00:00:12:34:01"
      name: eth-trunk
  ```

- `*.yml` / `*.yaml` (außer `interfaces.yaml`) — [nmstate](https://nmstate.io/)
  Desired-State-Dokumente, werden per `nmstatectl apply` angewendet.
- `*.nmconnection` — rohe NetworkManager-Keyfiles, werden nach
  `/etc/NetworkManager/system-connections/` installiert und aktiviert.

Beide Formate (nmstate und NetworkManager-Keyfiles) können gleichzeitig
verwendet werden — es kommt nur darauf an, welche Dateiendung die jeweiligen
Keys in der ConfigMap haben.

Siehe [`manifests/11-configmap-network-config.yaml`](manifests/11-configmap-network-config.yaml).

## Ein Trunk statt einer NIC pro Tenant

Eine dedizierte physische/Multus-NIC pro Tenant skaliert nicht: bei z. B.
150 per BGP gekoppelten Tenants bräuchte die VM 150 zusätzliche
Interfaces, und jedes neue Interface erfordert einen VM-Neustart (KubeVirt
hängt Bridge-Interfaces nicht ohne Neustart hot-plug an) — das widerspricht
dem Ziel, einen Tenant einfach per ConfigMap-Commit hinzufügen zu können.

Deshalb hat die VM nur **zwei** zusätzliche Interfaces, unabhängig von der
Anzahl der Tenants:

- `eth-trunk` — eine einzelne NIC, angebunden über eine Linux-Bridge-
  `NetworkAttachmentDefinition` **ohne** `vlan`-Feld: die Bridge-CNI legt
  das VM-Interface dadurch als Trunk-Port an, statt es auf eine feste VLAN-ID
  zu taggen/untaggen, und reicht 802.1Q-tagged Frames unverändert durch.
  Jeder Tenant bekommt sein eigenes VLAN — nicht sein eigenes Interface.
  Voraussetzung ist eine Node-seitige Bridge mit `vlan_filtering: true`
  (siehe Kommentar in der `NetworkAttachmentDefinition`).
- `eth-lan` — das interne, nicht tenant-spezifische Uplink-Interface.

Siehe [`manifests/20-networkattachmentdefinition.yaml`](manifests/20-networkattachmentdefinition.yaml)
für das Trunk-`NetworkAttachmentDefinition` und
[`manifests/30-virtualmachine.yaml`](manifests/30-virtualmachine.yaml) für
die VM-Seite.

## VRF pro Tenant und Policy-Based Routing

Jeder Tenant bekommt in `nmstate.yml` ein eigenes VLAN-Sub-Interface auf
`eth-trunk` sowie ein eigenes VRF, in das dieses Sub-Interface gesteckt
wird (`vrf-tenant1`, `vrf-tenant2`, …), sodass dessen Routing-Tabelle — und
die darin laufende BGP-Session — vollständig von den anderen Tenants und
vom Default-VRF isoliert ist:

```yaml
interfaces:
  - name: vrf-tenant1
    type: vrf
    state: up
    vrf:
      port:
        - tenant1
      route-table-id: 1001
  - name: tenant1
    type: vlan
    state: up
    vlan:
      base-iface: eth-trunk
      id: 100
    ipv4: { ... }
```

In `frr.conf` läuft entsprechend pro Tenant eine eigene BGP-Instanz
(`router bgp <ASN> vrf vrf-tenant1`, mit `neighbor ... bfd` für schnelle
Ausfallerkennung — siehe `bfdd=yes` in `daemons`), die die für dieses VRF
vorgesehenen Netze per `network`-Statement advertised.

Damit diese Netze aber tatsächlich über das jeweilige Tenant-VLAN
verlassen — auch wenn sie physisch an einem anderen Interface hängen (im
Beispiel: `eth-lan` im Default-VRF) — braucht es zwei weitere, ebenfalls
per `nmstate.yml` konfigurierte Bausteine (**nicht** FRRs `pbrd`: ein
`pbr-map` müsste an ein festes Ingress-Interface gebunden werden, ein
nmstate/Kernel-`route-rule` dagegen matcht rein auf die Quell-Adresse,
unabhängig vom Interface — das ist der deutlich besser skalierende Ansatz
bei vielen Tenants):

1. Eine geleakte Route je Tenant-VRF (`routes.config`, mit `table-id` auf
   die passende `route-table-id` des VRF), damit das `network`-Statement
   der jeweiligen BGP-Instanz überhaupt etwas zum Advertisen hat:

   ```yaml
   routes:
     config:
       - destination: 10.0.0.0/24
         next-hop-address: 10.0.2.254
         next-hop-interface: eth-lan
         table-id: 1001
   ```

2. Eine Policy-Routing-Regel je Netz (`route-rules.config`), die Pakete
   anhand ihrer Quell-Adresse der Routing-Tabelle des passenden VRF
   zuweist:

   ```yaml
   route-rules:
     config:
       - ip-from: 10.0.0.0/24
         priority: 1000
         route-table: 1001
   ```

Kurz gesagt: das VRF sorgt für die Isolation der BGP-Session, die geleakte
Route sorgt dafür, dass BGP das Netz kennt, und die `route-rule` sorgt
dafür, dass der tatsächliche Forwarding-Pfad für dieses Netz durch das
richtige VRF (und damit über das richtige Tenant-VLAN) läuft. Siehe
[`manifests/11-configmap-network-config.yaml`](manifests/11-configmap-network-config.yaml)
für das vollständige Beispiel.

## Tenants ohne VM-Neustart hinzufügen

Das ist der Regelfall beim Skalieren auf viele (z. B. ~150) Tenants und
berührt weder die `VirtualMachine` noch die `NetworkAttachmentDefinition` —
nur die `network-config` und `frr-config` ConfigMaps:

1. Freie VLAN-ID innerhalb der in der `trunk`-`NetworkAttachmentDefinition`
   erlaubten Range wählen (`vlan.trunk`, ggf. dort erweitern, falls
   ausgeschöpft — das ist der einzige Schritt, der die
   `NetworkAttachmentDefinition` berührt, und auch dafür ist kein
   VM-Neustart nötig).
2. In `network-config`s `nmstate.yml` das Tripel aus VLAN-Sub-Interface
   (`type: vlan`, `base-iface: eth-trunk`, `vlan.id: <ID>`), VRF-Interface
   und den passenden `routes`-/`route-rules`-Einträgen ergänzen (siehe
   oben).
3. In `frr-config`s `frr.conf` die passende `router bgp ... vrf ...`-Instanz
   für den neuen Tenant ergänzen.

`frr-config-sync.path` und `network-config-sync.path` übernehmen beide
Änderungen automatisch und live — `nmstatectl apply` legt das neue
VLAN-Sub-Interface an, ohne die bestehenden Interfaces oder laufenden
BGP-Sessions anderer Tenants zu stören. Bei dieser Größenordnung werden die
ConfigMap-Inhalte sinnvollerweise generiert (Helm/Kustomize/eigenes
Skript) statt von Hand gepflegt; am Format der ConfigMaps selbst ändert
das nichts.

## Ein neues physisches Interface zielsicher hinzufügen

Im Unterschied dazu ist das Hinzufügen einer komplett neuen physischen NIC
(z. B. ein zweiter Trunk für mehr Bandbreite oder eine weitere
Uplink-Redundanz) selten und erfordert tatsächlich einen VM-Neustart, da
KubeVirt Bridge-Interfaces nicht hot-plugged. Das Kernproblem dabei ist,
dass die Reihenfolge, in der der Gast neue NICs sieht (und damit der vom
Kernel vergebene Name wie `enp2s0`), nicht garantiert stabil ist. Deshalb
pinnt dieses Image Interface-Namen an MAC-Adressen (siehe oben), und der
Workflow ist bewusst zweigleisig, damit beide Seiten (VM-Spec und
Guest-Konfiguration) exakt zusammenpassen:

1. **MAC-Adresse festlegen.** Wählt eine feste, eindeutige MAC-Adresse für
   das neue Interface (z. B. aus dem lokal verwalteten Bereich `02:xx:xx:xx:xx:xx`).
2. **VM-Spec erweitern:** In der `VirtualMachine` unter
   `spec.template.spec.domain.devices.interfaces` einen neuen Eintrag mit
   `name` und genau dieser `macAddress` hinzufügen, dazu unter
   `spec.template.spec.networks` das passende `multus.networkName`
   (siehe [`manifests/20-networkattachmentdefinition.yaml`](manifests/20-networkattachmentdefinition.yaml)).
3. **`network-config` ConfigMap erweitern:** In `interfaces.yaml` einen
   Eintrag mit derselben MAC-Adresse und dem gewünschten Namen ergänzen,
   und in `nmstate.yml` (oder einer `*.nmconnection`-Datei) die
   Konfiguration für genau diesen Namen hinterlegen.
4. **VM neu starten.** Erst danach greift `frr-bootc-ifnaming.service`, das
   vor NetworkManager läuft und das neue Interface umbenennt, bevor es von
   NetworkManager beansprucht wird.

Damit ist das Hinzufügen eines physischen Interfaces "zielsicher": Der Name
im Gast hängt ausschließlich von der MAC-Adresse ab, die im VM-Spec
explizit gesetzt wurde — nicht von der PCI-Slot-Reihenfolge, in der
KubeVirt Interfaces anhängt.

> Änderungen an bereits vorhandenen Interfaces (IP-Adressen, Routing,
> neue VLAN-Sub-Interfaces auf einem bestehenden Trunk) über
> `nmstate.yml`/`*.nmconnection` werden dagegen **ohne Neustart** über
> `network-config-sync.path` live übernommen.

## Hinweise für Hochdurchsatz und Redundanz

Das Beispiel-Manifest ist bewusst minimal gehalten; für Produktivbetrieb
mit hohem Durchsatz oder Redundanzanforderungen fehlen ihm absichtlich
(und daher hier nur als Hinweis, nicht als fertiges Manifest):

- **Durchsatz:** `networkInterfaceMultiqueue: true` ist bereits gesetzt.
  Für sehr hohen Durchsatz (z. B. im zweistelligen Gbit/s-Bereich) kommen
  zusätzlich `spec.domain.cpu.dedicatedCpuPlacement`, Hugepages
  (`spec.domain.memory.hugepages`) und ggf. SR-IOV-`NetworkAttachmentDefinition`s
  für die Trunk-/LAN-Interfaces statt `bridge: {}` infrage — das braucht
  passende Node-Ressourcen (isolierte CPUs, Hugepage-Pool, SR-IOV-fähige
  NICs) und ist daher clusterspezifisch.
- **Redundanz:** Das Manifest zeigt eine einzelne `VirtualMachine`. Für ein
  n-Instanzen-Redundanzmodell (z. B. zwei VMs auf unterschiedlichen Nodes,
  BFD zwischen ihnen bzw. zu den Tenants für schnelles Failover) müsste man
  mehrere `VirtualMachine`-Objekte mit `podAntiAffinity` (auf
  `kubevirt.io/domain`) über verschiedene Nodes/Verfügbarkeitszonen
  verteilen — im Beispiel-Scope bewusst ausgeklammert.

## Build

Voraussetzung: `podman` mit Zugriff auf ein privilegiertes
`bootc-image-builder`-Setup.

```console
$ ./build.sh [tag]
```

Das Skript:

1. baut das bootc-Image aus `Containerfile`,
2. wandelt es mit `bootc-image-builder` in ein `qcow2` um,
3. verpackt das `qcow2` als minimales `containerDisk`-Image
   (`containerdisk/Containerfile`), dem Format, das KubeVirt für
   `spec...volumes[].containerDisk.image` erwartet.

Anschließend das `containerDisk`-Image in eine für den Cluster erreichbare
Registry pushen und in der `VirtualMachine` referenzieren.

### CI: bootc-Image automatisch bauen

Das bootc-OCI-Image (`Containerfile`) wird in CI gebaut und veröffentlicht,
lokal ist `./build.sh` nur für den zusätzlichen `containerDisk`-Schritt
nötig (der ein privilegiertes `bootc-image-builder`-Setup braucht und daher
nicht Teil der Pipelines ist):

- **GitHub Actions** ([`.github/workflows/build.yml`](.github/workflows/build.yml)):
  baut mit `docker/build-push-action` und pusht nach
  `ghcr.io/<owner>/<repo>` — bei jedem Push auf `main`, bei Tags (`v*.*.*`)
  und als reiner Build-Check auf Pull Requests (ohne Push).
- **GitLab CI** ([`.gitlab-ci.yml`](.gitlab-ci.yml)): baut mit
  [Kaniko](https://github.com/GoogleContainerTools/kaniko) (kein
  privilegierter Runner nötig) und pusht in die projekteigene Container
  Registry (`$CI_REGISTRY_IMAGE`) — bei Push auf den Default-Branch und bei
  Tags, als reiner Build-Check auf Merge Requests (`--no-push`).

## Deploy

Voraussetzung: OpenShift Virtualization mit aktiviertem virtiofs-Feature-Gate
für beliebige Volumes (ConfigMap/Secret/ServiceAccount als Filesystem) —
siehe Kommentar in [`manifests/30-virtualmachine.yaml`](manifests/30-virtualmachine.yaml).

```console
$ oc apply -f manifests/00-namespace.yaml
$ oc apply -f manifests/10-configmap-frr-config.yaml
$ oc apply -f manifests/11-configmap-network-config.yaml
$ oc apply -f manifests/20-networkattachmentdefinition.yaml   # falls zusätzliche NICs benötigt werden
$ oc apply -f manifests/30-virtualmachine.yaml                # <registry>/... vorher anpassen
```

Konfigurationsänderungen danach einfach per `oc edit configmap/frr-config`
bzw. `oc edit configmap/network-config -n frr-bootc` vornehmen — die
Sync-Services in der VM übernehmen den Rest.

## Fehlersuche

- `oc logs`/Konsolenzugriff auf die VM, dann in der VM:
  `journalctl -u frr-config-sync.service -u network-config-sync.service -u frr-bootc-ifnaming.service`
- Aktuell angewendeter Konfigurations-Hash: `/var/lib/frr-bootc/*.sha256`
- FRR-Validierungsfehler landen zusätzlich in `/tmp/frr-config-check.log`
  innerhalb der VM.
- `nmstatectl show` bzw. `nmcli connection show` zur Prüfung des aktuellen
  Netzwerkzustands.
