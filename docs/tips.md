- wayland
    - constrain to window
        - pointer-constraints-unstable-v1
    - https://wayland.app/protocols/linux-dmabuf-v1
        - initial setup, not per frame
    - https://wayland.app/protocols/linux-drm-syncobj-v1
        - semaphores galore
- which devices support which DRM formats
    - https://drmdb.emersion.fr/formats
    - spoiler: XRGB8888
- dmabuf device ID
    - under hyprland
        - 0xe201 -> card1 -> AMD
    - under cage in VT or nested in hyprland
        - 0xe280 -> renderD128 -> AMD (mismatched card/render ordering)
    - under greetd, cage, WLR_DRM_DEVICES
        - 0xe281 -> renderD129 -> nvidia
* superposition score
    * 27k-ish with waygriff running
    * 37749 in headless nvidia xorg
    * 30859 in wayland with PRIME
    * 19695 with fullscreen waygriff
    * 15342 with fullscreen waygriff in NVFBC nowait mode
* capture benchmark
    * cuda memcpy to staging buffer, blit to sysram buffer
        * 2.14ms per frame
    * cuda memset+scale to staging buffer, copy to sysram buffer
        * 1.62ms per frame