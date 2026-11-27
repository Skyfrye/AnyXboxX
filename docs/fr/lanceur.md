# Lanceur graphique : installation et utilisation

Version anglaise : [../launcher.md](../launcher.md). Code : `crates/anyxboxx-launcher`.

`anyxboxx-launcher.exe` est une fenêtre qui vérifie votre machine et votre dossier du jeu, compile ou trouve VIctory, et lance le jeu avec les réglages mesurés de cette lignée (`anyxboxx run --profile diffbase` sur NVIDIA). Il ne contient ni ne télécharge aucun fichier du jeu, et n'écrit jamais dans le dossier du jeu.

## Ce qu'il faut

- Windows 10 ou 11 x64, un processeur avec AVX2, une carte Direct3D 12 avec le shader model 6.6 et les mesh shaders (une carte NVIDIA RTX est recommandée : cette lignée est mesurée sur une RTX 5080), 32 Go de RAM. Liste complète : [lancer-gta6.md](lancer-gta6.md#prérequis).
- Votre propre copie du build Xbox Series 1.0.2.1, obtenue légalement : un dossier qui contient `game_scarlett_master.exe`. Non fournie.
- [Git](https://git-scm.com/), [Rust via rustup](https://rustup.rs/), Visual Studio 2022 ou 2026 Build Tools avec la charge « Développement Desktop en C++ ». Le lanceur compile VIctory depuis les sources avec cargo au premier appui sur PLAY, sauf si un `victory.exe` se trouve à côté de lui.
- De la place : la compilation de VIctory prend quelques Go et le cache des shaders grossit en jouant. Gardez plus de 30 Go libres sur le disque système (à 10 Go, le chargement ne finit jamais).

## Installation

```powershell
git clone --recurse-submodules https://github.com/Skyfrye/anyXboxx-diffbase.git
cd anyXboxx-diffbase
cargo build --release -p anyxboxx-launcher
```

Lancez ensuite `target\release\anyxboxx-launcher.exe`. VIctory est inclus dans `third_party/victory` : il n'y a rien d'autre à télécharger.

Le lanceur trouve le dépôt en remontant depuis son propre dossier jusqu'à rencontrer `third_party\victory\Cargo.toml`. Si vous gardez l'exécutable ailleurs, donnez le dossier du clone dans `ANYXBOXX_REPO` (`setx ANYXBOXX_REPO C:\chemin\vers\anyXboxx-diffbase`, puis ouvrez une nouvelle fenêtre). Un `victory.exe` posé à côté d'`anyxboxx-launcher.exe` est utilisé tel quel, et aucun dépôt n'est alors nécessaire ; `--victory-exe <chemin>` fait de même pour un `victory.exe` ailleurs.

## Premier lancement

1. Double-cliquez sur `anyxboxx-launcher.exe`.
2. Page **Setup** : chaque ligne doit être verte ou expliquée. Rust, le runtime Direct3D 12, le redistribuable VC++ et le pilote du GPU sont vérifiés ; le `dxc.exe` du Windows SDK ne sert qu'aux tests GPU du dépôt. Les liens mènent seulement aux pages officielles ; le lanceur n'installe rien lui-même.
3. Page **Game folder** : choisissez le dossier qui contient `game_scarlett_master.exe` (sélecteur de dossier, dépôt sur la fenêtre ou détection automatique). La vérification rapide ne lit que les fichiers de ce dossier.
4. **Home** : appuyez sur **PLAY**. La première fois, un écran plein montre les étapes : compilation de VIctory (plusieurs minutes, une seule fois), puis démarrage du jeu.
5. Le jeu démarre dans sa propre fenêtre et le lanceur se ferme, sauf si « Keep the launcher open » est activé.

Le premier lancement après un nouveau build ou un nouveau réglage traduit des centaines de shaders et tourne bien en dessous de 1 fps pendant un moment : jugez la vitesse au deuxième lancement. Pour les traduire plutôt avant de jouer, activez « Translate the shaders before playing » sur la page Play (il lance `victory shaders` sur les archives du jeu quand le cache n'est plus à jour), ou appuyez sur « Warm now » sur la page Shaders.

## Options

- **Display** : la taille de sortie (`VICTORY_OUTPUT`). « Screen size », par défaut, est la plus grande des tailles du titre (1280x720, 1664x936, 1920x1080, 2560x1440, 3840x2160) qui tient dans l'écran principal. « Native render » (`VICTORY_RENDER_SCALE=1.0`, activé par défaut) rend à cette taille au lieu des deux tiers. « Title's own » laisse les deux non définis : les 3840x2160 de la console, rendus aux deux tiers. Mode de fenêtre : fenêtré ou plein écran sans bordure.
- **Controls** : disposition du clavier (`VICTORY_KEYBOARD`), et les touches en jeu : cliquez sur l'image pour prendre la souris (caméra, gâchettes), F9 pour la prendre ou la lâcher, F3 pour les statistiques.
- **Features** :
  - Statistiques à l'écran (`VICTORY_PANEL=1`, activé) : fréquence d'images, temps d'image, mémoire et textures sur l'image ; F3 les montre ou les cache.
  - Lumière en ray tracing (`VICTORY_BVH_BUILDER=1`) : le constructeur de BVH du titre, intérieurs bien éclairés, environ 14 fps.
  - Yeux naturels (`VICTORY_AMBIENT_SPECULAR=1.5`, seulement sans ray tracing) : l'éclairage ambiant de remplacement borne les reflets.
  - Correctif des confettis sur la peau (`VICTORY_FIRST_UPLOAD_FRONT=1`, expérimental).
  - NVIDIA DLSS (`VICTORY_DLSS=1`, NVIDIA seulement, expérimental et pas mesuré en jeu) : votre propre `nvngx_dlss.dll` ou son dossier (`VICTORY_DLSS_DLL`, « Browse » le choisit), et le mode (`VICTORY_DLSS_MODE` : Quality, Balanced, Performance, Ultra performance, DLAA).
- **Performance** : Fast (le profil lui-même), Quality (fenêtres de données de dessin relues à chaque image, moins de copies sur la carte), carte de 12 Go (1024 Mo de tampons et 512 Mo de fenêtres réservés sur la carte) ou de 8 Go (512 Mo de tampons, pas de fenêtres, 85 % du budget de la carte pour les textures). Seul Fast est mesuré. Traduction des shaders avant de jouer, son nombre de workers, appuis automatiques sur Entrée/A pendant les écrans de démarrage (`VICTORY_KEYS`), NVIDIA Smooth Motion.
- **Diagnostics** : niveau de journal de VIctory, « Keep the launcher open », la vérification du jeu déjà lancé, et le dossier des journaux.
- Panneau **Advanced** : remplacement de variables `VICTORY_*` une par une ; ils l'emportent toujours sur le profil et les options.

NVIDIA Smooth Motion (RTX 40 et 50) est la génération d'images du pilote. Ce n'est pas une variable de VIctory : à chaque PLAY, le lanceur l'écrit dans le profil du pilote NVIDIA qui contient `victory.exe` (créé s'il n'existe pas), via NVAPI avec les identifiants de réglage qu'utilise NVIDIA Profile Inspector (`0xB0D384C0` activation, `0xB0CC0875` API autorisées : Direct3D 12). Aucun autre réglage du pilote n'est touché, et aucun droit administrateur n'est nécessaire.

Les réglages sont des profils par hôte et par fabricant de GPU (`crates/anyxboxx-launcher/profiles.toml`). Windows + NVIDIA utilise les réglages mesurés de cette lignée (`anyxboxx::run::DIFFBASE`) : ceux de `play.ps1` sans `VICTORY_CARD_AWAKE`, plus les budgets sur la carte, l'allègement de la RAM, l'ordre des placements et les interrupteurs du thread de soumission. Windows + AMD utilise les réglages de `scripts/play.ps1` de VIctory, mesurés sur une RX 7900 XTX. Linux (Proton) + AMD ajoute quatre valeurs mesurées (RX 9070 XT) et demande `tools/linux` ([lancer-gta6.md](lancer-gta6.md)). Intel et les autres GPU gardent les réglages par défaut de VIctory.

## Fichiers écrits

| Chemin | Contenu |
| --- | --- |
| `logs\game-*.log` à côté du lanceur | Le journal de chaque partie : la commande et chaque réglage de VIctory, puis la sortie de VIctory. Quand ce dossier ne peut pas être écrit : `%LOCALAPPDATA%\AnyXboxX-diffbase\logs` |
| `%LOCALAPPDATA%\AnyXboxX-diffbase\launcher\settings.toml` | Dossier du jeu et options |
| `%LOCALAPPDATA%\AnyXboxX\launcher\splash\` | Facultatif : vos propres images PNG ou JPEG, montrées en diaporama à la place de celles du lanceur |
| `%LOCALAPPDATA%\victory\data\` | Sauvegardes, données et `crashes.log` de VIctory |
| `third_party\victory\target\release\` | Le build de VIctory |
| Réglages du pilote NVIDIA | Avec Smooth Motion activé ou désactivé : les réglages Smooth Motion du profil qui contient `victory.exe` (`AnyXboxX (victory.exe)` quand le lanceur le crée) |

## Dépannage

- **« repository not found »** : l'exécutable est hors du clone, `ANYXBOXX_REPO` n'est pas défini, et aucun `victory.exe` n'est à côté de lui.
- **La compilation de VIctory échoue** : lancez `cargo build --release -p victory --manifest-path third_party\victory\Cargo.toml` dans un terminal pour voir le message de cargo ; il manque le plus souvent la charge MSVC.
- **La compilation du lanceur échoue sur `xma`** : le sous-module TomBox manque ; lancez `git submodule update --init`.
- **Un jeu tourne déjà** : le lanceur n'autorise qu'un jeu à la fois, car deux partageraient le dossier des sauvegardes.
- **La fenêtre s'est fermée toute seule** : le journal dit `window closed: suspending the title` quand vous l'avez fermée ; un plantage est écrit dans `crashes.log` sous `%LOCALAPPDATA%\victory\data\`.
- **Erreur Windows 1455 dans `crashes.log`** : le fichier d'échange est trop petit ; fermez d'autres applications ou agrandissez-le.
- **DLSS ne s'active pas** : le journal dit pourquoi (pas de `nvngx_dlss.dll` trouvé, une carte sur laquelle NGX ne fait pas tourner DLSS) ; l'upscaler du titre tourne alors comme avant.

Sous Proton ou Wine, le lanceur ne peut pas compiler VIctory (pas de cargo) : utilisez `anyxboxx run` comme décrit dans [lancer-gta6.md](lancer-gta6.md).
