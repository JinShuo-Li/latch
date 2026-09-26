MSYS restricted-token compatibility guard
========================================

Source: https://github.com/inmny/dsh-git-bash
Revision: 3a254f6e74e65ef6bb249852216e5ac9004c1636
License: MIT (`LICENSE.upstream`)

The guard and hook sources are included to make the Windows Git Bash process
boundary reproducible from source. The hook changes only MSYS runtime IPC ACL
construction and token-default-DACL handling; the outer runner must still
create and verify a restricted token, filesystem grants, and a kill-on-close
Job Object. Detours is MIT-licensed by Microsoft; see `vendor/detours/LICENSE.md`.
