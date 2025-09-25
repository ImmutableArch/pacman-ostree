# pacman-ostree
<p aligin="center">
    <img src="repo_content/logo.png" alt="Project Logo" width="200"/>
</p>

# 
pacman-ostree is a Hybrid OSTree Image/Pacman Package installer ⚛️/📦 written in Rust, inspired by rpm-ostree
```mermaid
flowchart TD
    pacmanostree["pacman-ostree (daemon + CLI)
        status, upgrade, rollback
        package layering
        initramfs --enable"] 
    ostree["ostree (image system)
        fetch ostree repositories
        transactional upgrades and rollbacks"]
    pacman["pacman (Arch Linux Package Manager)  package managent
        "]

    pacmanostree --> ostree
    pacmanostree --> pacman
```
# Roadmap
- [X] Create github repo
- [ ] Add pacman helpers on rust
- [ ] Add compose function
- [ ] Add base function (commit, deploy)
- [ ] Add pacman package layering
- [ ] Add upgrade function
- [ ] Add Deamon (pacman-ostreed)
