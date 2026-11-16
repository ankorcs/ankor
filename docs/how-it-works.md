---
title: How it works
description: The parts of an Ankor setup, what a node stores, and a match from launch to finish.
slug: how-it-works
---

An Ankor setup has three sides:

- A controller, which organises matches - either your own or the official ankor.sh controller.
- Game nodes, which run the matches on CS2 servers.
- The players, who join those servers in CS2.

The sections below show how the sides fit together and follow one match from its launch to its
end. The other guides use the terms that this page defines, so read it before you
[install a node](install.md) or start on the [protocol](protocol/index.md).

## The moving parts

The diagram shows a controller and one game node with two slots. One slot runs a match, and the
other runs a server without a match.

<figure class="diagram" aria-label="The controller at the top; a game node below it, holding the agent, the CS2 installation and two slots, each running a game server; players at the bottom joining the game servers.">
<div class="diagram-card"><p class="diagram-name">Controller</p><p class="diagram-sub"><a href="https://ankor.sh">ankor.sh</a> or your own</p><p class="diagram-text">Lobbies, teams and maps; statistics built from uploaded events.</p></div>
<div class="diagram-link from-agent"><span>The agent connects</span></div>
<div class="diagram-place"><span class="diagram-place-label">Game node<small>a Linux host with a node token</small></span>
<div class="diagram-row">
<div class="diagram-card"><p class="diagram-name">Agent</p><p class="diagram-text">Starts and stops game servers and keeps their events.</p></div>
<div class="diagram-card"><p class="diagram-name">CS2 installation</p><p class="diagram-text">One copy, shared by every slot.</p></div>
</div>
<div class="diagram-row">
<div class="diagram-place"><span class="diagram-place-label">Slot <code>s1</code><small>game 27015 · GOTV 27020</small></span>
<div class="diagram-card"><p class="diagram-name">Match</p><p class="diagram-sub">CS2 and the Ankor plugin</p><dl class="diagram-fields"><dt>allocation</dt><dd>5b1e2c7a-8d4f-…</dd><dt>login token</dt><dd>7K3Q9XW2M8RT…</dd><dt>match id</dt><dd>0199a0b4-5c00-…</dd></dl></div>
</div>
<div class="diagram-place"><span class="diagram-place-label">Slot <code>s2</code><small>game 27016 · GOTV 27021</small></span>
<div class="diagram-card"><p class="diagram-name">Server without a match</p><p class="diagram-sub">CS2, the node's plugins and cfgs</p><dl class="diagram-fields"><dt>allocation</dt><dd>9c04e1d2-3a7b-…</dd><dt>login token</dt><dd>Q4T8N2WX7C1R…</dd></dl></div>
</div>
</div>
</div>
<div class="diagram-link quiet"><span>Players join on the game port</span></div>
<div class="diagram-card"><p class="diagram-name">Players</p><p class="diagram-text">In CS2.</p></div>
</figure>

The rest of these docs use the terms below.

<div class="parts">
<p class="parts-title">Terminology</p>

<div class="part"><span class="part-icon" aria-hidden="true"><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="10" /> <path d="M12 2a14.5 14.5 0 0 0 0 20 14.5 14.5 0 0 0 0-20" /> <path d="M2 12h20" /></svg></span>

### Controller

The controller organises matches and decides which server runs on which node. A node starts a
server only when its controller asks for one.

A controller is any service that accepts the agent's WebSocket connection and serves the HTTP
endpoints that the node uploads to. It is typically a web service, such as
[ankor.sh](https://ankor.sh) or one that you build against the [protocol](protocol/index.md). The
node finds its controller through the `controller` keys in
[`agent.yml`](configuration.md#agentyml), the node's configuration file.

</div>

<div class="part"><span class="part-icon" aria-hidden="true"><svg viewBox="0 0 24 24" fill="currentColor"><path d="M12.504 0c-.155 0-.315.008-.48.021-4.226.333-3.105 4.807-3.17 6.298-.076 1.092-.3 1.953-1.05 3.02-.885 1.051-2.127 2.75-2.716 4.521-.278.832-.41 1.684-.287 2.489a.424.424 0 00-.11.135c-.26.268-.45.6-.663.839-.199.199-.485.267-.797.4-.313.136-.658.269-.864.68-.09.189-.136.394-.132.602 0 .199.027.4.055.536.058.399.116.728.04.97-.249.68-.28 1.145-.106 1.484.174.334.535.47.94.601.81.2 1.91.135 2.774.6.926.466 1.866.67 2.616.47.526-.116.97-.464 1.208-.946.587-.003 1.23-.269 2.26-.334.699-.058 1.574.267 2.577.2.025.134.063.198.114.333l.003.003c.391.778 1.113 1.132 1.884 1.071.771-.06 1.592-.536 2.257-1.306.631-.765 1.683-1.084 2.378-1.503.348-.199.629-.469.649-.853.023-.4-.2-.811-.714-1.376v-.097l-.003-.003c-.17-.2-.25-.535-.338-.926-.085-.401-.182-.786-.492-1.046h-.003c-.059-.054-.123-.067-.188-.135a.357.357 0 00-.19-.064c.431-1.278.264-2.55-.173-3.694-.533-1.41-1.465-2.638-2.175-3.483-.796-1.005-1.576-1.957-1.56-3.368.026-2.152.236-6.133-3.544-6.139zm.529 3.405h.013c.213 0 .396.062.584.198.19.135.33.332.438.533.105.259.158.459.166.724 0-.02.006-.04.006-.06v.105a.086.086 0 01-.004-.021l-.004-.024a1.807 1.807 0 01-.15.706.953.953 0 01-.213.335.71.71 0 00-.088-.042c-.104-.045-.198-.064-.284-.133a1.312 1.312 0 00-.22-.066c.05-.06.146-.133.183-.198.053-.128.082-.264.088-.402v-.02a1.21 1.21 0 00-.061-.4c-.045-.134-.101-.2-.183-.333-.084-.066-.167-.132-.267-.132h-.016c-.093 0-.176.03-.262.132a.8.8 0 00-.205.334 1.18 1.18 0 00-.09.4v.019c.002.089.008.179.02.267-.193-.067-.438-.135-.607-.202a1.635 1.635 0 01-.018-.2v-.02a1.772 1.772 0 01.15-.768c.082-.22.232-.406.43-.533a.985.985 0 01.594-.2zm-2.962.059h.036c.142 0 .27.048.399.135.146.129.264.288.344.465.09.199.14.4.153.667v.004c.007.134.006.2-.002.266v.08c-.03.007-.056.018-.083.024-.152.055-.274.135-.393.2.012-.09.013-.18.003-.267v-.015c-.012-.133-.04-.2-.082-.333a.613.613 0 00-.166-.267.248.248 0 00-.183-.064h-.021c-.071.006-.13.04-.186.132a.552.552 0 00-.12.27.944.944 0 00-.023.33v.015c.012.135.037.2.08.334.046.134.098.2.166.268.01.009.02.018.034.024-.07.057-.117.07-.176.136a.304.304 0 01-.131.068 2.62 2.62 0 01-.275-.402 1.772 1.772 0 01-.155-.667 1.759 1.759 0 01.08-.668 1.43 1.43 0 01.283-.535c.128-.133.26-.2.418-.2zm1.37 1.706c.332 0 .733.065 1.216.399.293.2.523.269 1.052.468h.003c.255.136.405.266.478.399v-.131a.571.571 0 01.016.47c-.123.31-.516.643-1.063.842v.002c-.268.135-.501.333-.775.465-.276.135-.588.292-1.012.267a1.139 1.139 0 01-.448-.067 3.566 3.566 0 01-.322-.198c-.195-.135-.363-.332-.612-.465v-.005h-.005c-.4-.246-.616-.512-.686-.71-.07-.268-.005-.47.193-.6.224-.135.38-.271.483-.336.104-.074.143-.102.176-.131h.002v-.003c.169-.202.436-.47.839-.601.139-.036.294-.065.466-.065zm2.8 2.142c.358 1.417 1.196 3.475 1.735 4.473.286.534.855 1.659 1.102 3.024.156-.005.33.018.513.064.646-1.671-.546-3.467-1.089-3.966-.22-.2-.232-.335-.123-.335.59.534 1.365 1.572 1.646 2.757.13.535.16 1.104.021 1.67.067.028.135.06.205.067 1.032.534 1.413.938 1.23 1.537v-.043c-.06-.003-.12 0-.18 0h-.016c.151-.467-.182-.825-1.065-1.224-.915-.4-1.646-.336-1.77.465-.008.043-.013.066-.018.135-.068.023-.139.053-.209.064-.43.268-.662.669-.793 1.187-.13.533-.17 1.156-.205 1.869v.003c-.02.334-.17.838-.319 1.35-1.5 1.072-3.58 1.538-5.348.334a2.645 2.645 0 00-.402-.533 1.45 1.45 0 00-.275-.333c.182 0 .338-.03.465-.067a.615.615 0 00.314-.334c.108-.267 0-.697-.345-1.163-.345-.467-.931-.995-1.788-1.521-.63-.4-.986-.87-1.15-1.396-.165-.534-.143-1.085-.015-1.645.245-1.07.873-2.11 1.274-2.763.107-.065.037.135-.408.974-.396.751-1.14 2.497-.122 3.854a8.123 8.123 0 01.647-2.876c.564-1.278 1.743-3.504 1.836-5.268.048.036.217.135.289.202.218.133.38.333.59.465.21.201.477.335.876.335.039.003.075.006.11.006.412 0 .73-.134.997-.268.29-.134.52-.334.74-.4h.005c.467-.135.835-.402 1.044-.7zm2.185 8.958c.037.6.343 1.245.882 1.377.588.134 1.434-.333 1.791-.765l.211-.01c.315-.007.577.01.847.268l.003.003c.208.199.305.53.391.876.085.4.154.78.409 1.066.486.527.645.906.636 1.14l.003-.007v.018l-.003-.012c-.015.262-.185.396-.498.595-.63.401-1.746.712-2.457 1.57-.618.737-1.37 1.14-2.036 1.191-.664.053-1.237-.2-1.574-.898l-.005-.003c-.21-.4-.12-1.025.056-1.69.176-.668.428-1.344.463-1.897.037-.714.076-1.335.195-1.814.12-.465.308-.797.641-.984l.045-.022zm-10.814.049h.01c.053 0 .105.005.157.014.376.055.706.333 1.023.752l.91 1.664.003.003c.243.533.754 1.064 1.189 1.637.434.598.77 1.131.729 1.57v.006c-.057.744-.48 1.148-1.125 1.294-.645.135-1.52.002-2.395-.464-.968-.536-2.118-.469-2.857-.602-.369-.066-.61-.2-.723-.4-.11-.2-.113-.602.123-1.23v-.004l.002-.003c.117-.334.03-.752-.027-1.118-.055-.401-.083-.71.043-.94.16-.334.396-.4.69-.533.294-.135.64-.202.915-.47h.002v-.002c.256-.268.445-.601.668-.838.19-.201.38-.336.663-.336zm7.159-9.074c-.435.201-.945.535-1.488.535-.542 0-.97-.267-1.28-.466-.154-.134-.28-.268-.373-.335-.164-.134-.144-.333-.074-.333.109.016.129.134.199.2.096.066.215.2.36.333.292.2.68.467 1.167.467.485 0 1.053-.267 1.398-.466.195-.135.445-.334.648-.467.156-.136.149-.267.279-.267.128.016.034.134-.147.332a8.097 8.097 0 01-.69.468zm-1.082-1.583V5.64c-.006-.02.013-.042.029-.05.074-.043.18-.027.26.004.063 0 .16.067.15.135-.006.049-.085.066-.135.066-.055 0-.092-.043-.141-.068-.052-.018-.146-.008-.163-.065zm-.551 0c-.02.058-.113.049-.166.066-.047.025-.086.068-.14.068-.05 0-.13-.02-.136-.068-.01-.066.088-.133.15-.133.08-.031.184-.047.259-.005.019.009.036.03.03.05v.02h.003z"/></svg></span>

### Game node

A game node, or node, is a Linux host that you control and that runs game servers. Each node has a
**node token**, which identifies the node to its controller.

</div>

<div class="part"><span class="part-icon" aria-hidden="true"><svg viewBox="0 0 24 24" fill="currentColor"><path d="M13.983 11.078h2.119a.186.186 0 00.186-.185V9.006a.186.186 0 00-.186-.186h-2.119a.185.185 0 00-.185.185v1.888c0 .102.083.185.185.185m-2.954-5.43h2.118a.186.186 0 00.186-.186V3.574a.186.186 0 00-.186-.185h-2.118a.185.185 0 00-.185.185v1.888c0 .102.082.185.185.185m0 2.716h2.118a.187.187 0 00.186-.186V6.29a.186.186 0 00-.186-.185h-2.118a.185.185 0 00-.185.185v1.887c0 .102.082.185.185.186m-2.93 0h2.12a.186.186 0 00.184-.186V6.29a.185.185 0 00-.185-.185H8.1a.185.185 0 00-.185.185v1.887c0 .102.083.185.185.186m-2.964 0h2.119a.186.186 0 00.185-.186V6.29a.185.185 0 00-.185-.185H5.136a.186.186 0 00-.186.185v1.887c0 .102.084.185.186.186m5.893 2.715h2.118a.186.186 0 00.186-.185V9.006a.186.186 0 00-.186-.186h-2.118a.185.185 0 00-.185.185v1.888c0 .102.082.185.185.185m-2.93 0h2.12a.185.185 0 00.184-.185V9.006a.185.185 0 00-.184-.186h-2.12a.185.185 0 00-.184.185v1.888c0 .102.083.185.185.185m-2.964 0h2.119a.185.185 0 00.185-.185V9.006a.185.185 0 00-.184-.186h-2.12a.186.186 0 00-.186.186v1.887c0 .102.084.185.186.185m-2.92 0h2.12a.185.185 0 00.184-.185V9.006a.185.185 0 00-.184-.186h-2.12a.185.185 0 00-.184.185v1.888c0 .102.082.185.185.185M23.763 9.89c-.065-.051-.672-.51-1.954-.51-.338.001-.676.03-1.01.087-.248-1.7-1.653-2.53-1.716-2.566l-.344-.199-.226.327c-.284.438-.49.922-.612 1.43-.23.97-.09 1.882.403 2.661-.595.332-1.55.413-1.744.42H.751a.751.751 0 00-.75.748 11.376 11.376 0 00.692 4.062c.545 1.428 1.355 2.48 2.41 3.124 1.18.723 3.1 1.137 5.275 1.137.983.003 1.963-.086 2.93-.266a12.248 12.248 0 003.823-1.389c.98-.567 1.86-1.288 2.61-2.136 1.252-1.418 1.998-2.997 2.553-4.4h.221c1.372 0 2.215-.549 2.68-1.009.309-.293.55-.65.707-1.046l.098-.288Z"/></svg></span>

### Agent

The agent is the one long-running service on a node. It runs in a Docker container named
`ankor-agent`, beside the game servers. The agent connects to the controller and carries out its
requests. Through Docker, the agent installs and updates CS2 and starts and stops the game servers.

</div>

<div class="part"><span class="part-icon" aria-hidden="true"><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round"><rect width="20" height="8" x="2" y="2" rx="2" ry="2" /> <rect width="20" height="8" x="2" y="14" rx="2" ry="2" /> <line x1="6" x2="6.01" y1="6" y2="6" /> <line x1="6" x2="6.01" y1="18" y2="18" /></svg></span>

### Slots and game servers

A **slot** is room for one game server on a node. Each slot has an id, a game port, a GOTV port
and a [login token](requirements.md#game-server-login-tokens). GOTV is the CS2 feature that lets
spectators watch a server and that records demos.

A **game server**, or **server**, is the CS2 process that a [launch](#kinds-of-launch) starts in a
slot. Each server runs in a separate Docker container. The controller picks an **allocation id**
for each launch, and the node names the server and its events by that id.

A **match** is a server that was launched with a match configuration. Ankor's plugin runs the
match for one map.

</div>

<div class="part"><span class="part-icon" aria-hidden="true"><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round"><path d="M15.39 4.39a1 1 0 0 0 1.68-.474 2.5 2.5 0 1 1 3.014 3.015 1 1 0 0 0-.474 1.68l1.683 1.682a2.414 2.414 0 0 1 0 3.414L19.61 15.39a1 1 0 0 1-1.68-.474 2.5 2.5 0 1 0-3.014 3.015 1 1 0 0 1 .474 1.68l-1.683 1.682a2.414 2.414 0 0 1-3.414 0L8.61 19.61a1 1 0 0 0-1.68.474 2.5 2.5 0 1 1-3.014-3.015 1 1 0 0 0 .474-1.68l-1.683-1.682a2.414 2.414 0 0 1 0-3.414L4.39 8.61a1 1 0 0 1 1.68.474 2.5 2.5 0 1 0 3.014-3.015 1 1 0 0 1-.474-1.68l1.683-1.682a2.414 2.414 0 0 1 3.414 0z" /></svg></span>

### Plugin

The plugin is Ankor's plugin for [SwiftlyS2](https://swiftlys2.net), a plugin framework for CS2
servers. It runs in every game server, and the [kind of launch](#kinds-of-launch) decides what it
does. The plugin talks only to the agent on its node, on
[port [limit:EVENT_RELAY_PORT]](#host-networking). Any
[SwiftlyS2 plugins](operations.md#add-swiftlys2-plugins) that you add to the node load beside it.

</div>

<div class="part"><span class="part-icon" aria-hidden="true"><svg viewBox="0 0 24 24" fill="currentColor"><path d="M21.7101 3.2348a.0218.0218 0 0 1-.0216-.0215c.001-.0815.003-.3698.004-.424 0-.1292-.1428-.1831-.2117-.0829-.0113.016-.1457.212-.2287.3323a.0244.0244 0 0 1-.0198.0103h-6.5508a.0473.0473 0 0 1-.0473-.0463l-.0131-.177a.0477.0477 0 0 1 .056-.0477l.335.0319a.059.059 0 0 0 .0624-.0445l.2441-.989a.0475.0475 0 0 0-.0295-.0542l-.2268-.0848a.0427.0427 0 0 1-.0263-.0295c-.0412-.1722-.3766-1.3235-1.9935-1.581-.7867-.125-1.302.2107-1.577.4784a1.594 1.594 0 0 0-.3018.4095l-.0965.2125c-.008.0164-.046.2157-.046.234l.0513.9815a.109.109 0 0 0 .0422.0856l.3546.153-.1958.3244a.055.055 0 0 1-.053.0402s-.417.0108-.6227.0192c-.3856.0155-1.2444.4858-1.8773 1.8385-.6219 1.3282-.724 1.5496-.724 1.5496a.0736.0736 0 0 1-.0684.0398l-.5776.0019c-.0356 0-.0736.0285-.0886.0608l-.8982 2.5428c-.0149.0323-.006.081.0173.1081l.627.3918a.059.059 0 0 1 .0195.059l-.3274.9664a.1916.1916 0 0 1-.0235.0618l-.4343.3824a.1056.1056 0 0 0-.0356.059l-.5978 1.5308a.0632.0632 0 0 1-.0608.0455l-.3355.0018a.1633.1633 0 0 0-.162.1488l-.2007 2.2883a1.7194 1.7194 0 0 1-.0159.1207l-.1583.908a.128.128 0 0 1-.0333.0553l-.5584.4263c-.2559.2443-.5988.6903-.7665 1.0015l-1.86 3.9235c-.045.0833-.0811.227-.0788.3221l.1321.2354c.002.0842-.0319.4559-.0707.5307L.817 23.6545a.0985.0985 0 0 0-.003.0856l.0309.0706.0932.1859 1.8913.0029c.1173.0106.247-.1396.2507-.3005l.1027-1.2965-.0268-.1952 3.6063-4.232c.0942-.1146.222-.3168.2861-.4506l1.7186-3.7892a.1712.1712 0 0 1 .1004-.088l.1086-.035a.1694.1694 0 0 1 .1827.0528c.1505.1807.5042.781.6765 1.0315.1425.2079.8495 1.2304 1.1582 1.5675.0853.0926.3481.198.4658.2696a.083.083 0 0 1 .029.1119l-1.0298 1.8083-.4549 2.1357a.9542.9542 0 0 0-.0356.1526l-.4118 1.4831c.003.1873-.141.2856-.1527.5064l-.15 1.084a.0581.0581 0 0 0 .0582.0614l2.5445.014c.0951-.0003.1902-.0045.2854-.0067a1.1049 1.1049 0 0 0 .0755-.0069c.1242-.0158.5629-.0754.75-.1503.1097-.0388.1809-.0819.2268-.1295.1855-.1935.2002-.278.2034-.3975.001-.0212-.011-.0726-.0283-.1049-.0053-.0117-.0316-.0382-.059-.0477l-1.1815-.3562a.3693.3693 0 0 1-.1889-.1338l-.3172-.469a.0865.0865 0 0 1 .0173-.0973l.618-.609a.2017.2017 0 0 0 .0483-.072l1.9036-4.488c.089-.285.0595-.6056 0-.9445-.044-.2504-.6854-1.3264-.8532-1.6236-.1476-.262-1.0677-1.8707-1.286-2.2517-.0793-.1376-.1894-.1328-.2287-.276l-.0726-1.1173a.0398.0398 0 0 1 .0356-.0505l.3312-.0276a.093.093 0 0 0 .0741-.0487l1.147-2.1544a.0958.0958 0 0 0-.002-.094l-.2349-.2907a.0874.0874 0 0 1-.001-.0875l.3515-.3796a.054.054 0 0 1 .0736-.0206l.9343.5266a.3819.3819 0 0 0 .186.0505c.259-.0019.6858-.1549.908-.2906a.3833.3833 0 0 0 .1386-.148l.4583-1.0703c.006-.0136.0262-.0117.029.0028l.1278.5953a.0635.0635 0 0 0 .0788.0501l1.3494-.3a.0662.0662 0 0 0 .05-.0786l-.318-1.3437a.0716.0716 0 0 1 .009-.0534l.1313-.2036a.281.281 0 0 0 .036-.0814l.1589-.725a.0408.0408 0 0 1 .0397-.0323l3.7327.0047a.0916.0916 0 0 0 .0932-.0931v-.6336a.022.022 0 0 1 .0216-.0216h1.4393a.0465.0465 0 0 0 .0464-.0463v-.2855a.0465.0465 0 0 0-.0464-.0463h-1.4398z"/></svg></span>

### CS2 installation

Each node has one CS2 installation, which the servers in every slot share.
[One installation, many servers](#one-installation-many-servers) explains how they share it.

</div>

</div>

## Kinds of launch

A launch is the controller's request to start a game server, which it sends as
[agent:start_server]. The launch sets the game mode, the player count, the cfgs and the console
settings. Cfgs are CS2 config files, which hold console commands for the server to run. A launch
can also carry a match.

A match can carry a **match id**, which is the controller's identifier for the match. The match id
names the demos of the match. Every match reports its phase and score to the controller over the
agent's connection, in [agent:phase_change] and [agent:score]. If a match sends **statistics**, the
node also uploads its gameplay events to the controller. The match sends statistics when its
`send_statistics` field is `true`.

Whether a launch carries a match, and whether that match sends statistics, decides what the server
does:

| Launch                      | Ankor's plugin                                                                                                                                                              | Phase and score                 | Gameplay events             |
|-----------------------------|-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------|---------------------------------|-----------------------------|
| Match that sends statistics | Runs the roster and warmup for one map, and the knife round and [demo recording](operations.md#demos) when the [match](/reference/types/#schema-MatchConfig) asks for them. | Reported on the agent's socket. | Uploaded to the controller. |
| Match without statistics    | Runs the same match flow as a match that sends statistics.                                                                                                                  | Reported on the agent's socket. | Not collected.              |
| No match                    | Runs no match. The map, the cfgs, the console settings and the node's plugins set the game.                                                                                 | Not reported.                   | Not collected.              |

A match can continue on a new server from the backup of a live round.
[Launch a game](protocol/index.md#6-launch-a-game) explains how. A match must play a stock map,
which every CS2 installation includes, in the `casual`, `competitive` or `wingman` game mode. A
launch without a match can play a map from the Steam Workshop, in any
[game mode](/reference/types/#schema-GameMode).

## Series

A match covers one map. Its launch carries no information about a series, such as the number of
maps, the place of the map in the series or the score so far.

A best-of-three is therefore three matches, with three match ids, which the controller launches one
after another. The controller runs the veto, the side picks and the series score. It also launches
each next map.

Other match plugins, such as get5 and MatchZy, play a whole series on one server. That server
changes maps and keeps the series score. Ankor leaves the series to the controller instead, because
each game server shuts down when its map ends. This choice has these trade-offs:

<p class="choice-label pros">Pros</p>

- **Any map, any node.** The controller can send each map to whichever node has a free slot. If a
  node fails between maps, no series state is lost, because the node stores none.
- **One kind of game server.** A map in a series runs like any other map. The plugin has no map
  change, series state or end-of-series handling.
- **Formats are the controller's.** Bo2, Bo7, a map pool or a per-map side rule needs no new
  release of the plugin or the agent.

<p class="choice-label cons">Cons</p>

- **More to build.** If your controller offers series, it must implement the veto, the series score
  and the hand-over between maps.
- **Players reconnect between maps,** because each map runs on a new server.
- **No series in the records a node sends.** Demos carry only their match id, and events carry only
  the allocation id of their server. The controller groups them into a series.

## From launch to finished match

These steps follow one server from the controller's request to the end of its match:

1. **Launch.** The controller asks a node to start a server with [agent:start_server]. The agent
   claims a free slot and starts CS2 with the plugin. If the node has no free slot, it refuses the
   launch, and the controller tries another node.
2. **Ready.** When the server has logged on to Steam and loaded its map, the plugin reports that
   players can join. The agent then reports the slot as `running` in [agent:slot_status]. The
   controller gives the players the address of the server.
3. **Play.** In a match that sends statistics, the plugin reports gameplay events. The agent stores
   them on the node and forwards them to the controller.
4. **Finish.** A match ends when its map ends. The controller can stop any server earlier. The
   agent also stops a server that stays empty for the `idle_shutdown_seconds` of its launch. The
   server shuts down once the agent has stored its last events. The agent then frees the slot and
   [uploads the demo](protocol/demos.md), if the match recorded one.

The [protocol overview](protocol/index.md) shows the messages behind each step, and the
[reference](/reference/) describes each message.

## Control and history

The agent and the controller exchange two kinds of traffic, which the node handles differently:

- **Live control** covers launches, commands and server status. The node does not store it. After
  a reconnect, the agent reports its current state again.
- **Gameplay history** stays stored on the node until the controller confirms that it has received
  it. [Event uploads](protocol/events.md) describes how the node uploads events and how the
  controller confirms them.

## Host networking

Game servers and the agent run in Docker's
[`host` network mode](https://docs.docker.com/engine/network/drivers/host/). They use the node's
network stack instead of a private Docker network behind NAT.

Game traffic is a steady stream of small UDP packets. Host networking keeps Docker's address
translation and port forwarding out of that path. Players therefore connect directly to the server
on the address of the node. Each plugin connects to the agent on loopback TCP port
[limit:EVENT_RELAY_PORT].

The diagram shows the connections of a node with two slots. The agent opens the connection to the
controller, so the controller never connects to the node.

<figure class="diagram" aria-label="The agent opens a connection out to the controller on TCP 443; on the node, each plugin talks to the agent on loopback port [limit:EVENT_RELAY_PORT]; players reach each slot's game server on its UDP game and GOTV ports.">
<div class="diagram-card"><p class="diagram-name">Controller</p><p class="diagram-text">Never connects to the node.</p></div>
<div class="diagram-link from-agent"><span>TCP 443<small>WebSocket and uploads, opened by the agent</small></span></div>
<div class="diagram-place"><span class="diagram-place-label">The node's own network<small>host network mode</small></span>
<div class="diagram-card"><p class="diagram-name">Agent</p><p class="diagram-sub"><code>127.0.0.1:[limit:EVENT_RELAY_PORT]</code></p></div>
<div class="diagram-link"><span>TCP [limit:EVENT_RELAY_PORT], loopback only<small>each plugin dials its agent</small></span></div>
<div class="diagram-row">
<div class="diagram-card"><p class="diagram-name">Slot <code>s1</code></p><p class="diagram-sub">UDP 27015 · GOTV 27020</p></div>
<div class="diagram-card"><p class="diagram-name">Slot <code>s2</code></p><p class="diagram-sub">UDP 27016 · GOTV 27021</p></div>
</div>
</div>
<div class="diagram-link quiet"><span>UDP game and GOTV ports<small>forwarded to the node behind NAT</small></span></div>
<div class="diagram-card"><p class="diagram-name">Players</p><p class="diagram-text">Connect to the node's public IPv4 address.</p></div>
</figure>

## One installation, many servers

Every game server on a node reads the same CS2 installation, so the node downloads CS2 only once.
The plugin and its configuration sit in a separate folder, called the overlay. The agent copies the
overlay out of the `ankor-cs2` image and mounts it on top of the installation in every server. The
agent keeps one overlay for each `ankor-cs2` image. A CS2 update therefore never overwrites the
plugin, and the plugin never changes the game files.

The installation must not change while a server reads it. A CS2 update therefore waits until every
slot on the node is idle. [Update CS2](operations.md#update-cs2) describes how the node updates CS2
and what it needs for an update.
[Node size versus node count](requirements.md#node-size-versus-node-count) compares many small
nodes with a few large ones, which this wait affects.

## Updates and restarts

The table shows when each change to a node can happen and what it does to the servers that are
running:

| Change                                | When                                                            | Running servers                                                                                                                              |
|---------------------------------------|-----------------------------------------------------------------|----------------------------------------------------------------------------------------------------------------------------------------------|
| CS2 update                            | Only when every slot is [idle](#one-installation-many-servers). | None run during the update.                                                                                                                  |
| CS2 image (`ankor-cs2`) replaced      | Any time.                                                       | Keep the image that they started with. New servers start on the new image.                                                                   |
| Agent restarted or its image replaced | Any time.                                                       | Keep running. The new agent takes over the running servers and their stored events. If the replacement fails, the previous agent runs again. |
| Game server crashes                   | Without warning.                                                | Keep the events that the agent has stored, and lose the rest. A relaunch starts a new server.                                                |
| Host restarts                         | Any time.                                                       | End as in a crash. Docker starts the agent again.                                                                                            |
| Connection to the controller lost     | Any time.                                                       | Keep running. Their events wait on the node.                                                                                                 |

The node updates CS2 and its images when the controller asks. While [config:auto_update] is on, as
it is by default, the node also updates them by itself. [Images](operations.md#images) lists
which versions of the agent and CS2 images work together.
[Automatic updates](operations.md#automatic-updates) describes what the node updates by itself and
how to turn that off.

## Trust

The agent controls Docker on the host through the Docker socket, which gives the agent root
access to the host. These rules decide which images a node runs, and what a controller and a game
server can reach:

- By default, a node runs only the official images that the
  [Ankor GitHub repository](https://github.com/ankorcs/ankor) publishes, as
  [config:official_images_only] describes. The agent refuses [agent:update_agent] and
  [agent:update_cs2_image] for any other image, and the setup script refuses to run one. Only the
  node's operator can turn this setting off, in `agent.yml`.
- If the setting is off, [agent:update_agent] runs any image that the controller sends, and that
  image can use the Docker socket. Connect such a node only to a controller that you would give
  root on that host.
- While [config:auto_update] is on, the node runs each new release or rebuild of its
  [release line](operations.md#images), from the image repositories that `agent.yml` names.
- A server needs a Steam Web API key to download a map from the Steam Workshop. The key arrives in
  the launch of the server, so whoever runs the node can read the key.
- Game containers run with dropped capabilities and without access to the Docker socket.
- The agent listens for plugins on loopback only, and it accepts only the servers that it launched.
