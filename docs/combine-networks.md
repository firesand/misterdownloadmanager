# Combine networks

**Downloads → Combine networks…** makes file downloads go through several networks at once:
Wi-Fi and Ethernet, a phone tethered over USB, a second line. Each network gets connections of
its own (as many as **Options → Connection** allows for one download), held to its adapter, so
the networks' speeds add up. The free connections split what is left of a file, so a faster
network ends up carrying more of it.

It only helps when each network reaches the internet its own way. Wi-Fi and Ethernet plugged
into the same router share one line: the total stays the same.

## The window

- **Combine the ticked networks for file downloads** turns it on. The networks that reach the
  internet by themselves (Ethernet, Wi-Fi, USB) are ticked when first seen; a VPN's adapter is
  listed, unticked. Bridges and containers' adapters are left out.
- Untick a network while downloads run and its connections close within half a second; the
  others go on with its part. Tick it again and it rejoins. A ticked network that connects
  later (a phone plugged in) joins the running downloads too.
- With every ticked network off or disconnected, downloads wait, keeping what they have, and go
  on when one is back.
- **Settings…** (or a double-click) gives the selected network a name of your own, a speed
  limit for all downloads together, and a data allowance per day, week or month (a phone
  plan's). Once the allowance is used up the network is left out until the next period begins
  (weeks start on Monday); **Reset** counts the period again from nothing.

While networks are combined, a download's progress window names the network of each connection,
and the status strip under the list shows how fast each network is.

## What it covers

File downloads over HTTP and HTTPS use every ticked network. So does a video that comes as one
file, or as one file for the picture and one for the sound (YouTube's qualities, most sites'
MP4s): yt-dlp finds it and Mister Download Manager downloads it, as it does a file.

The rest is spread differently:

- A server that can't send parts of a file, an FTP download, and a site that only lets a
  browser-like connection through use the first ticked network that is connected.
- So do a video sent in pieces (HLS, DASH) and an image gallery's pictures. What they bring
  is not counted against that network's data allowance or held to its own speed limit.
- Usenet spreads its connections over the networks connected when it starts.
- Torrents, and what yt-dlp and gallery-dl fetch themselves (looking a page up, a live
  stream), go through the one adapter chosen in **Options → Proxy / Socks → Network
  adapter**, as before.

## From the command line

```
misterdownloadmanager-cli get https://example.com/big.iso --combine eno2,wlp0s20f3
```

`--combine` takes the adapters' names as `--adapter` does, separated by commas or repeated.
The summary at the end says how much each network carried. `video-get` takes it too:

```
misterdownloadmanager-cli video-get https://example.com/watch/1 --combine eno2,wlp0s20f3
```

Where it comes from: [Plexo](https://github.com/anmolkapil/plexo), whose idea and behaviour
this follows; the code is Mister Download Manager's own.
