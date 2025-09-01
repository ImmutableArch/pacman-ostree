# pacman-ostree
<p aligin="center">
    <img src="repo_content/logo.png" alt="Project Logo" width="200"/>
</p>

# 
pacman-ostree is a Hybrid OSTree Image/Pacman Package installer ⚛️/📦 written in C, inspired by rpm-ostree
```mermaid
flowchart TD
    pacmanostree["pacman-ostree (daemon + CLI)
        status, upgrade, rollback
        package layering
        initramfs --enable"] 
    bootc["bootc (image system)
        fetch bootable container images
        transactional upgrades and rollbacks"]
    ostree["ostree (image system)
        fetch ostree repositories
        transactional upgrades and rollbacks"]
    alpm["alpm (Arch Linux Package Managent) ties together
        "]

    pacmanostree --> ostree
    pacmanostree --> bootc
    pacmanostree --> libdnf
```
# Roadmap
- [X] Create github repo
- [ ] Add base function (commit, deploy)
- [ ] Add alpm package layering
- [ ] Add upgrade function
- [ ] Add Deamon (pacman-ostreed)
