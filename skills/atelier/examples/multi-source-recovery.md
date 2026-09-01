# Multi-source landing and parked recovery

```sh
mkdir product-workspace
cd product-workspace
atelier init
atelier attach ~/work/api --mount api
atelier attach ~/work/web --mount web
atelier session open --summary "Rename the shared plan across API and web"
```

Edit `api/...` and `web/...` inside the printed working copy, then:

```sh
atelier session diff s1
atelier land s1
```

If `api` lands and `web` parks after another actor moved the web line:

```sh
# Resolve only web's overlap inside s1's existing working copy.
atelier session diff s1
atelier land s1
atelier requests
atelier journal
```

Do not run `atelier approve <request>` on the parked snapshot. The second
`land s1` is the transition that snapshots the resolution and re-opens the
gate. The retry skips `api`, which already landed.

After every source lands, publish adopted Git branches normally:

```sh
git -C api push origin <branch>
git -C web push origin <branch>
```
