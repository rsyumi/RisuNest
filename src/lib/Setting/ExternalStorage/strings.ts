import type {
    ExternalConnectionSummary,
    ExternalProviderId,
} from 'src/ts/storage/sync/external/types'

const english = {
    title: 'External storage',
    help: 'Upload backups to a cloud service or your own server, and optionally keep several devices in sync through one of them. Data is encrypted on this device before it is uploaded.',
    unsupported: 'External storage is available in the Android and desktop apps.',
    add: 'Connect storage', cancel: 'Cancel', refresh: 'Refresh', loading: 'Loading…', back: 'Back to the form',
    provider: 'Service', mode: 'Repository', create: 'Create new', existing: 'Connect existing repository',
    existingHelp: 'Connecting an existing repository needs the recovery key of that repository.',
    purpose: 'Purpose', backup: 'Backup only', sync: 'Synchronization',
    purposeHelp: 'Backup only uploads and never downloads; it downloads only when you restore by hand. Synchronization keeps uploading and downloading so that the latest data is everywhere, but there is no real-time synchronization.',
    backupOnlyProvider: 'This service supports backups only.',
    backupOnly: 'Backup only',
    requestUsage: 'Estimated requests used on this device',
    connectionInfo: 'Connection details', endpoint: 'Server address', profile: 'Service type', optional: ' (optional)',
    scope: 'Data to store', library: 'Characters, chats and attachments',
    hypa: 'Hypa embedding data', deviceSettings: 'Local settings', devicePlugins: 'Local plugin data',
    scopeHelp: 'Choose what this connection backs up. You can change it after connecting.',
    libraryHelp: 'Characters, chats and attachments are stored by default.',
    hypaHelp: 'Embedding data Hypa uses. These values can be produced again with the embedding API.',
    devicePluginsHelp: 'Data plugins store per device. Restoring brings plugin settings back as they were.',
    deviceSettingsHelp: "This device's sync, update and performance settings, and its plugin permissions. Used when restoring to the same device.",
    included: 'Included',
    chooseRestore: 'Choose what to restore',
    syncPurposeLocalDataNotice: 'Characters, chats and attachments are synchronized. Whether local data is synchronized with them is chosen under Local data in the RisuNest settings, and each device can choose differently.',
    prepare: 'Review details',
    pendingVerification: 'The next step reviews what you entered and asks you to sign in or to enter the password or key. Pressing Review details does not connect to the server.',
    sequentialTitle: 'Use one device at a time',
    sequentialWarning: 'Use one device at a time. Confirm that synchronization has finished and close the app before using another device.',
    githubWarningTitle: 'Create a separate private repository for backups',
    githubWarning: 'This connection keeps creating draft releases and tags. Do not connect a repository you use for anything else.',
    acknowledge: 'I understand',
    endpointReview: 'Where this connects', authority: 'Server', account: 'Account', repository: 'Repository location', purposeReview: 'Purpose', includes: 'includes {0}',
    confirmEndpoint: 'This server and folder are correct', connect: 'Connect', signIn: 'Sign in', finishSignIn: 'Finish sign-in',
    connectHint: 'Connect contacts the server, creates the folder and then shows your recovery key.',
    recoveryRequiredTitle: 'This repository needs its recovery key',
    recoveryRequired: 'The repository is encrypted. Open the recovery file saved when it was created, or paste its contents (or the QR code contents), and enter the recovery code.',
    recoveryPayload: 'Recovery file or QR contents', recoveryPayloadPlaceholder: 'Open the recovery file and paste its contents', recoveryCode: 'Recovery code', unlock: 'Check recovery key',
    openRecoveryFile: 'Open recovery file',
    noConnections: 'No external storage is connected. Press Connect storage to connect WebDAV, an S3-compatible bucket, Google Drive, OneDrive, NAVER MYBOX, GitHub or GitLab.',
    activeSync: 'Sync repository', makeSyncTarget: 'Sync with this repository',
    runBackup: 'Back up now', runSync: 'Sync now', history: 'History', quota: 'Storage usage', recovery: 'Recovery key', remove: 'Disconnect',
    restore: 'Restore', download: 'Save as file', pin: 'Keep from cleanup', pinned: 'Kept', local: 'Keep this device', remote: 'Keep repository', conflicts: 'Conflicts',
    lastSync: 'Last sync', lastBackup: 'Last backup', progress: 'Progress',
    conflictTitle: 'The same data changed on this device and in the repository',
    remotePendingTitle: 'The repository contents have not been received yet',
    preservationPending: 'The contents of this device are kept in history. The repository contents must be received before you can choose which side to keep.',
    preservationComplete: 'The repository copy was confirmed. Choose an available side to keep, or restore a preserved copy.',
    conflictResolvedHelp: 'The preserved copies remain available until you delete this conflict backup.',
    thisDevice: 'This device', repositorySide: 'Repository', remotePending: 'Not received yet', retryPreservation: 'Receive repository contents again',
    copyUnavailable: 'Unavailable', conflictResolved: 'Conflict resolved', restoreLocalCopy: 'Restore device copy', restoreRemoteCopy: 'Restore repository copy',
    exportLocalCopy: 'Save device copy', exportRemoteCopy: 'Save repository copy',
    deleteConflict: 'Delete', deleteConflictConfirm: 'Delete this conflict backup?', remoteDeletePending: 'The local copy was deleted. The repository copy remains.',
    saveRecovery: 'Save recovery file', showRecovery: 'Recovery QR code', closeRecovery: 'Close',
    recoveryCodeHelp: 'Needed together with the QR code. Write it down somewhere else.',
    recoveryFileOnly: 'This recovery key is too large for a QR code. Save it as a file and load that file when recovering.',
    platformClientId: 'OAuth client ID for this device', oauthProjectHint: 'OAuth project',
    authorizationUnavailable: 'Signing in needs a native iOS callback, which is not available in this build.',
    oneDriveAndroidSetup: 'On Android, add risunestlocal://oauth/onedrive under Mobile and desktop applications and enable public client flows in the Entra app.',
    googleAndroidSetup: 'In Google Cloud, create a Web OAuth client, add the exact HTTPS callback address below, and enable the Google Drive API. After signing in, the callback page returns to RisuNest by itself.',
    googleIOSSetup: 'In Google Cloud, create an iOS OAuth client with bundle ID io.github.rsyumi.risunest and enable the Google Drive API.',
    oneDriveIOSSetup: 'In the Entra app, add an iOS/macOS platform with bundle ID io.github.rsyumi.risunest and enable public client flows.',
    iosOAuthClientId: 'iOS OAuth client ID',
    webOAuthClientId: 'Web OAuth client ID', oauthCallbackUrl: 'Sign-in completion address (HTTPS)', oauthClientSecret: 'Client secret (optional)',
    manualOAuthCallback: 'Sign-in completion page address (if the app does not return by itself)', manualOAuthHelp: 'If the app does not reopen by itself, paste the complete address of the sign-in completion page.',
    authorizationWaiting: 'Finish signing in in the browser and the app returns. If it does not, paste the complete address of the sign-in completion page.',
    callbackRejected: 'That address does not belong to this sign-in attempt. Check it and paste the complete address again.',
    recoveryNotice: 'This is the key that opens the backups in this repository. You need it to connect the repository from a new device or if you lose this one. Anyone who has both the recovery file and the recovery code can open the backups, so keep them in different, safe places.',
    completed: 'Completed', failed: 'Failed', queued: 'Queued', running: 'Working…', waiting: 'Waiting', uncertain: 'Could not confirm that the repository was updated. Run the same action again to check.',
    loadMore: 'Show older history', statusReady: 'Connected', statusPaused: 'Paused', statusReauth: 'Sign-in required', statusLocked: 'Recovery key required', statusError: 'Needs attention',
    usedByService: 'Used on the service', unknownUsage: 'Unknown', unknownUsageHelp: 'This service does not report it.', uploadedLowerBound: 'Uploaded from this device', latestReachable: 'Size of the latest backup', atLeast: '{0} or more', files: '{0} files or more',
    cleanup: 'Clean up now', retentionCount: 'Backups to keep', retentionDays: 'Keep for', retentionDaysUnit: 'days', lastCleanup: 'Last cleanup', cleanupNever: 'Not cleaned up yet',
    retentionHelp: 'An automatic backup made on this device is removed only once it is past both the number to keep and the length to keep. Kept items and conflict records are never removed.',
    retentionOtherDevices: 'Backups made on other devices are left alone.',
    cleanupSummary: 'Removed {0} files ({1}).',
    cleanupPartial: 'Some files are left for the next cleanup.',
    cleanupDeferred: 'Cleanup is waiting for another storage task to finish.',
    cleanupUncertain: 'Could not confirm that cleanup has finished. Storage tasks will wait until the result is confirmed.',
    cleanupTrashNotice: 'Some services move deleted files to a recycle bin, so the space may not be free right away.',
    invalidConfiguration: 'Some fields are empty or invalid. Check the connection details.',
    retry: 'Could not reach the repository. Check your internet connection and try again.', retryAction: 'Try again',
    reauthenticate: 'The sign-in expired or the key is no longer valid. Sign in again or enter a new key.',
    unlockKey: 'Enter the recovery key to use this repository.',
    resolveRequired: 'Resolve the conflict first. The Conflicts tab lets you choose which side to keep.',
    freeSpace: 'The repository is out of space. Free space on the service and try again.',
    credentialsRejected: 'The service did not accept this sign-in or key. Check what you entered and try again.',
    connectionAlreadyAdded: 'This storage is already connected.',
    repositoryNotFound: 'Could not find the repository. Check the folder, bucket and address.',
    stateChanged: 'The repository changed while this was running. Refresh and try again.',
    requestBudget: 'The service request limit was reached. Try again later.',
    objectTooLarge: 'A file is larger than this service allows.',
    corrupted: 'The stored data did not pass verification. Choose another backup or connect the repository again.',
    unsupportedOperation: 'This repository cannot do that.',
    interrupted: 'This stopped before it finished. Try again.',
    errorGeneric: 'Could not complete this. Try again.',
    oldDriveNote: 'Google Drive backup here uses the official RisuAI account method. To back up to your own Drive folder or another cloud, use External storage in the RisuNest tab.',
    jobKinds: { backup: 'Backup', sync: 'Sync', restore: 'Restore', 'pin-history': 'Keep', 'resolve-conflict': 'Resolve conflict' },
    jobActive: { backup: 'Backing up', sync: 'Syncing', restore: 'Restoring', 'pin-history': 'Keeping', 'resolve-conflict': 'Resolving conflict' },
    historyKinds: { snapshot: 'Sync', 'backup-point': 'Backup', conflict: 'Conflict backup', 'recovery-candidate': 'Recovery candidate' },
    endpointWarnings: {
        'github-dedicated-repository': 'Use a separate private repository. This connection is backup only.',
        'gitlab-cleanup-policy': 'GitLab package cleanup policies can delete backups. Keep cleanup policies off for this project. This connection is backup only.',
    },
    providers: {
        webdav: { name: 'WebDAV / Koofr', description: 'Connects to an HTTPS WebDAV folder with an application password.' },
        s3: { name: 'S3-compatible storage', description: 'Uses an S3-compatible bucket such as Amazon S3, Cloudflare R2, Backblaze B2 or Hugging Face.' },
        google_drive: { name: 'Google Drive', description: 'Signs in with a Google account and uses a Drive folder or the hidden app data space.', warningTitle: 'Take care with the hidden app data space', warning: 'If you choose the hidden app data space, deleting the app data in Drive also deletes the backups.' },
        onedrive: { name: 'OneDrive', description: 'Signs in with a Microsoft account and uses a personal, work or app-only folder.' },
        mybox: { name: 'NAVER MYBOX', description: 'Uses a MYBOX personal access token and a dedicated folder.' },
        github_releases: { name: 'GitHub Releases', description: 'Uploads encrypted backups to draft releases in a private repository. Backup only.' },
        gitlab_packages: { name: 'GitLab packages', description: 'Uploads encrypted backups to a dedicated package registry. Backup only.', warningTitle: 'GitLab cleanup policies can delete backups', warning: 'Keep package cleanup policies off for this project.' },
    },
    fields: {
        'webdav.accountId': 'User name', 'webdav.root': 'Folder name', 'webdav.password': 'Application password',
        's3.bucket': 'Bucket', 's3.prefix': 'Folder', 's3.region': 'Region', 's3.addressing': 'Addressing', 's3.accessKeyId': 'Access key ID', 's3.secretAccessKey': 'Secret access key',
        'google_drive.folderId': 'Folder ID', 'google_drive.space': 'Storage location', 'google_drive.oauthRedirectUri': 'Sign-in completion address (HTTPS)', 'google_drive.projectId': 'OAuth project ID', 'google_drive.clientId': 'OAuth client ID for this device',
        'onedrive.accountType': 'Account type', 'onedrive.tenant': 'Tenant', 'onedrive.driveId': 'Drive ID', 'onedrive.rootItemId': 'Folder item ID', 'onedrive.redirectUri': 'Sign-in completion address', 'onedrive.projectId': 'App client ID', 'onedrive.clientId': 'Client ID for this device',
        'mybox.rootFolderName': 'Folder name', 'mybox.rootFolderId': 'Existing folder ID', 'mybox.pat': 'Personal access token', 'mybox.expiresAtMs': 'Token expiry',
        'github_releases.uploadEndpoint': 'Upload address', 'github_releases.owner': 'Owner', 'github_releases.repo': 'Private repository name', 'github_releases.tagPrefix': 'Tag prefix', 'github_releases.token': 'Personal access token (fine-grained)',
        'gitlab_packages.projectId': 'Project ID or path', 'gitlab_packages.packageName': 'Package name', 'gitlab_packages.maxFileBytes': 'Maximum file size (bytes)', 'gitlab_packages.token': 'Access token',
    },
    fieldHelp: {
        'webdav.root': 'The repository is created inside this folder. Do not keep other files in it.',
        'webdav.password': 'An application password created in the service settings, not your account password.',
        'github_releases.token': 'Needs read and write access to the contents of the backup repository.',
        'gitlab_packages.token': 'Use a personal or project access token with api scope and Maintainer or Owner access to this project.',
    },
    options: {
        's3.addressing.': 'Service default', 's3.addressing.path': 'Path style', 's3.addressing.virtual': 'Virtual host style',
        'google_drive.space.drive': 'Visible Drive folder', 'google_drive.space.appDataFolder': 'Hidden app data',
        'onedrive.accountType.personal': 'Personal', 'onedrive.accountType.business': 'Work or school', 'onedrive.accountType.appFolder': 'App-only folder',
    },
    profiles: {
        'webdav.': 'Generic WebDAV', 's3.aws': 'Amazon S3', 's3.generic': 'Other S3-compatible', 'gitlab_packages.': 'Automatic', 'gitlab_packages.selfManaged': 'Self-managed', 'mybox.plan': '{0} plan',
    },
}

const korean: typeof english = {
    title: '외부 저장소',
    help: '클라우드나 개인 서버에 백업을 올리고, 원하면 그중 하나로 여러 기기를 동기화합니다. 올리는 데이터는 이 기기에서 암호화됩니다.',
    unsupported: '외부 저장소는 Android 및 데스크톱 앱에서 사용할 수 있습니다.',
    add: '저장소 연결', cancel: '취소', refresh: '새로 고침', loading: '불러오는 중…', back: '입력으로 돌아가기',
    provider: '서비스', mode: '저장소', create: '새로 만들기', existing: '이미 있는 저장소 연결',
    existingHelp: '이미 있는 저장소를 연결할 때는 그 저장소의 복구 키가 필요합니다.',
    purpose: '용도', backup: '백업만', sync: '동기화',
    purposeHelp: '백업만 하는 경우 데이터를 다운로드 하지 않고 업로드만 하며, 수동으로 복원하는 경우에만 다운로드를 합니다. 동기화를 하는 경우 업로드 다운로드를 반복하여 항상 최신 데이터로 동기화를 시도합니다. 다만 실시간 동기화 기능은 지원되지 않습니다.',
    backupOnlyProvider: '이 서비스는 백업만 지원합니다.',
    backupOnly: '백업만',
    requestUsage: '이 기기의 예상 요청 사용량',
    connectionInfo: '연결 정보', endpoint: '서버 주소', profile: '서비스 종류', optional: ' (선택)',
    scope: '저장할 데이터', library: '캐릭터·대화와 첨부 파일',
    hypa: '하이파 임베딩 데이터', deviceSettings: '로컬 설정', devicePlugins: '로컬 플러그인 데이터',
    scopeHelp: '이 연결에서 백업할 데이터를 선택하세요. 연결한 뒤에도 변경할 수 있습니다.',
    libraryHelp: '캐릭터, 대화, 첨부 파일은 기본적으로 저장됩니다.',
    hypaHelp: '하이파에서 쓰는 임베딩 데이터입니다. 임베딩 API로 다시 생성할 수 있는 값입니다.',
    devicePluginsHelp: '플러그인이 기기별로 저장하는 데이터입니다. 복원하면 플러그인 설정이 그대로 돌아옵니다.',
    deviceSettingsHelp: '이 기기의 동기화·업데이트·성능 설정과 플러그인 권한입니다. 같은 기기로 복원할 때 사용됩니다.',
    included: '포함',
    chooseRestore: '복원할 영역 선택',
    syncPurposeLocalDataNotice: '캐릭터, 대화, 첨부 파일이 동기화됩니다. 로컬 데이터를 함께 동기화할지는 RisuNest 설정의 로컬 데이터에서 선택하며, 기기마다 다르게 선택할 수 있습니다.',
    prepare: '입력 내용 확인',
    pendingVerification: '다음 단계에서 입력한 내용을 검토하고 로그인을 하거나, 비밀번호/키를 입력하게 됩니다. 입력 내용 확인 버튼을 눌러도 서버에 연결되지 않습니다.',
    sequentialTitle: '한 번에 한 기기에서만 사용하세요',
    sequentialWarning: '한 번에 한 기기에서 사용하세요. 다른 기기를 사용하기 전에 동기화 완료를 확인하고 앱을 닫아주세요.',
    githubWarningTitle: '백업 전용 비공개 저장소를 따로 만드세요',
    githubWarning: '이 연결은 초안 릴리스와 태그를 계속 만듭니다. 다른 용도로 쓰는 저장소에는 연결하지 마세요.',
    acknowledge: '확인했습니다',
    endpointReview: '연결할 곳', authority: '서버', account: '계정', repository: '저장소 위치', purposeReview: '용도', includes: '{0} 포함',
    confirmEndpoint: '이 서버와 폴더가 맞습니다', connect: '연결', signIn: '로그인', finishSignIn: '로그인 완료',
    connectHint: '연결을 누르면 서버에 접속해 폴더를 만들고 복구 키를 보여드립니다.',
    recoveryRequiredTitle: '이 저장소의 복구 키가 필요합니다',
    recoveryRequired: '암호화된 저장소입니다. 이 저장소를 만들 때 저장한 복구 파일을 불러오거나 그 내용(또는 QR 내용)을 붙여넣고, 복구 코드를 입력하세요.',
    recoveryPayload: '복구 파일 내용 또는 QR 내용', recoveryPayloadPlaceholder: '복구 파일을 열어 내용을 붙여넣으세요', recoveryCode: '복구 코드', unlock: '복구 키 확인',
    openRecoveryFile: '복구 파일 불러오기',
    noConnections: '연결된 외부 저장소가 없습니다. 저장소 연결을 눌러 WebDAV, S3 호환 저장소, Google Drive, OneDrive, 네이버 MYBOX, GitHub, GitLab 중 하나를 연결하세요.',
    activeSync: '동기화 중인 저장소', makeSyncTarget: '이 저장소로 동기화',
    runBackup: '지금 백업', runSync: '지금 동기화', history: '이력', quota: '저장소 용량', recovery: '복구 키', remove: '연결 해제',
    restore: '복원', download: '파일로 저장', pin: '지우지 않고 보관', pinned: '보관 중', local: '이 기기 내용 유지', remote: '저장소 내용 유지', conflicts: '충돌',
    lastSync: '마지막 동기화', lastBackup: '마지막 백업', progress: '진행',
    conflictTitle: '이 기기와 저장소에서 같은 데이터를 고쳤습니다',
    remotePendingTitle: '저장소 내용을 아직 받지 못했습니다',
    preservationPending: '이 기기 내용은 보관했습니다. 저장소 내용을 받아 와야 어느 쪽을 남길지 고를 수 있습니다.',
    preservationComplete: '저장소 사본을 확인했습니다. 사용할 수 있는 쪽을 유지하거나 보관된 사본을 복원하세요.',
    conflictResolvedHelp: '이 충돌 백업을 삭제하기 전까지 보관된 사본을 복원할 수 있습니다.',
    thisDevice: '이 기기', repositorySide: '저장소', remotePending: '아직 받지 못함', retryPreservation: '저장소 내용 다시 받기',
    copyUnavailable: '사용할 수 없음', conflictResolved: '충돌 해결됨', restoreLocalCopy: '기기 사본 복원', restoreRemoteCopy: '저장소 사본 복원',
    exportLocalCopy: '기기 사본 저장', exportRemoteCopy: '저장소 사본 저장',
    deleteConflict: '삭제', deleteConflictConfirm: '이 충돌 백업을 삭제하시겠습니까?', remoteDeletePending: '로컬 사본은 삭제됐으며, 저장소 사본은 남아 있습니다.',
    saveRecovery: '복구 파일 저장', showRecovery: '복구 QR 코드', closeRecovery: '닫기',
    recoveryCodeHelp: 'QR 코드와 함께 필요합니다. 다른 곳에 따로 적어 두세요.',
    recoveryFileOnly: '복구 키가 QR 코드에 담기지 않을 만큼 큽니다. 파일로 저장한 뒤, 복구할 때 그 파일을 불러오세요.',
    platformClientId: '이 기기용 OAuth 클라이언트 ID', oauthProjectHint: 'OAuth 프로젝트',
    authorizationUnavailable: '로그인에는 네이티브 iOS 콜백이 필요하지만 이 빌드에서는 사용할 수 없습니다.',
    oneDriveAndroidSetup: 'Android에서는 Entra 앱의 모바일 및 데스크톱 애플리케이션에 risunestlocal://oauth/onedrive를 추가하고 공용 클라이언트 흐름을 사용 설정하세요.',
    googleAndroidSetup: 'Google Cloud에서 웹 OAuth 클라이언트를 만들고 아래의 HTTPS 주소를 그대로 추가한 뒤 Google Drive API를 사용 설정하세요. 로그인이 끝나면 완료 페이지가 RisuNest로 자동으로 돌아옵니다.',
    googleIOSSetup: 'Google Cloud에서 번들 ID가 io.github.rsyumi.risunest인 iOS OAuth 클라이언트를 만들고 Google Drive API를 사용 설정하세요.',
    oneDriveIOSSetup: 'Entra 앱의 iOS/macOS 플랫폼에 번들 ID io.github.rsyumi.risunest를 추가하고 공용 클라이언트 흐름을 사용 설정하세요.',
    iosOAuthClientId: 'iOS OAuth 클라이언트 ID',
    webOAuthClientId: '웹 OAuth 클라이언트 ID', oauthCallbackUrl: '로그인 완료 주소 (HTTPS)', oauthClientSecret: '클라이언트 보안 비밀 (선택)',
    manualOAuthCallback: '로그인 완료 페이지 주소 (자동으로 돌아오지 않을 때)', manualOAuthHelp: '앱이 자동으로 다시 열리지 않으면 로그인 완료 페이지의 주소를 통째로 붙여넣으세요.',
    authorizationWaiting: '브라우저에서 로그인을 마치면 앱으로 돌아옵니다. 자동으로 돌아오지 않으면 로그인 완료 페이지의 주소를 통째로 붙여넣으세요.',
    callbackRejected: '이 주소는 현재 로그인 시도의 것이 아닙니다. 확인한 뒤 주소를 통째로 다시 붙여넣으세요.',
    recoveryNotice: '이 저장소의 백업을 여는 열쇠입니다. 새 기기에서 이 저장소를 연결하거나 이 기기를 잃었을 때 필요합니다. 복구 파일과 복구 코드가 둘 다 있으면 누구나 백업을 열 수 있으니, 서로 다른 곳에 안전하게 보관하세요.',
    completed: '완료', failed: '실패', queued: '대기열에 추가됨', running: '처리 중…', waiting: '대기 중', uncertain: '저장소에 반영됐는지 확인하지 못했습니다. 다시 확인하려면 같은 작업을 다시 실행하세요.',
    loadMore: '이전 이력 더 보기', statusReady: '연결됨', statusPaused: '일시 중지됨', statusReauth: '다시 로그인 필요', statusLocked: '복구 키 필요', statusError: '확인 필요',
    usedByService: '서비스에서 쓰는 용량', unknownUsage: '알 수 없음', unknownUsageHelp: '이 서비스는 알려주지 않습니다.', uploadedLowerBound: '이 기기에서 올린 데이터', latestReachable: '최신 백업 하나의 크기', atLeast: '{0} 이상', files: '파일 {0}개 이상',
    cleanup: '지금 정리', retentionCount: '보관 개수', retentionDays: '보관 기간', retentionDaysUnit: '일', lastCleanup: '마지막 정리', cleanupNever: '아직 정리하지 않았습니다',
    retentionHelp: '이 기기에서 만든 자동 백업은 보관 개수와 보관 기간을 모두 넘긴 경우에만 지웁니다. 「지우지 않고 보관」한 항목과 충돌 기록은 지우지 않습니다.',
    retentionOtherDevices: '다른 기기에서 만든 백업은 이 기기가 지우지 않습니다.',
    cleanupSummary: '{0}개를 지웠습니다 ({1}).',
    cleanupPartial: '남은 파일은 다음 정리에서 지웁니다.',
    cleanupDeferred: '다른 저장소 작업이 완료되기를 기다리고 있습니다.',
    cleanupUncertain: '정리 완료 여부를 확인할 수 없습니다. 결과가 확인될 때까지 저장소 작업이 대기합니다.',
    cleanupTrashNotice: '서비스에 따라 지운 파일이 휴지통으로 가며, 용량이 바로 줄지 않을 수 있습니다.',
    invalidConfiguration: '비어 있거나 잘못된 항목이 있습니다. 연결 정보를 확인하세요.',
    retry: '저장소에 연결하지 못했습니다. 인터넷 연결을 확인하고 다시 시도하세요.', retryAction: '다시 시도',
    reauthenticate: '로그인이 만료되었거나 키가 더 이상 유효하지 않습니다. 다시 로그인하거나 새 키를 입력하세요.',
    unlockKey: '복구 키를 입력해야 이 저장소를 쓸 수 있습니다.',
    resolveRequired: '충돌을 먼저 해결하세요. 충돌 탭에서 어느 쪽을 남길지 고를 수 있습니다.',
    freeSpace: '저장소 공간이 부족합니다. 서비스에서 공간을 확보한 뒤 다시 시도하세요.',
    credentialsRejected: '서비스가 이 로그인이나 키를 받아들이지 않았습니다. 입력한 내용을 확인하고 다시 시도하세요.',
    connectionAlreadyAdded: '이 저장소는 이미 연결되어 있습니다.',
    repositoryNotFound: '저장소를 찾지 못했습니다. 폴더·버킷과 주소를 확인하세요.',
    stateChanged: '작업하는 사이에 저장소가 바뀌었습니다. 새로 고침한 뒤 다시 시도하세요.',
    requestBudget: '서비스의 요청 한도에 걸렸습니다. 잠시 뒤 다시 시도하세요.',
    objectTooLarge: '이 서비스가 허용하는 크기보다 큰 파일이 있습니다.',
    corrupted: '저장된 데이터가 검증을 통과하지 못했습니다. 다른 백업을 고르거나 저장소를 다시 연결하세요.',
    unsupportedOperation: '이 저장소에서는 할 수 없는 작업입니다.',
    interrupted: '끝나기 전에 멈췄습니다. 다시 시도하세요.',
    errorGeneric: '작업을 마치지 못했습니다. 다시 시도하세요.',
    oldDriveNote: '여기의 Google Drive 백업은 RisuAI 공식 계정 방식입니다. 내 Drive 폴더나 다른 클라우드에 백업하려면 RisuNest 탭의 외부 저장소를 쓰세요.',
    jobKinds: { backup: '백업', sync: '동기화', restore: '복원', 'pin-history': '보관', 'resolve-conflict': '충돌 해결' },
    jobActive: { backup: '백업 중', sync: '동기화 중', restore: '복원 중', 'pin-history': '보관 중', 'resolve-conflict': '충돌 해결 중' },
    historyKinds: { snapshot: '동기화', 'backup-point': '백업', conflict: '충돌 백업', 'recovery-candidate': '복구 후보' },
    endpointWarnings: {
        'github-dedicated-repository': '백업 전용 비공개 저장소를 따로 쓰세요. 이 연결은 백업만 합니다.',
        'gitlab-cleanup-policy': 'GitLab의 패키지 정리 정책이 백업을 지울 수 있습니다. 이 프로젝트에서는 정리 정책을 꺼 두세요. 이 연결은 백업만 합니다.',
    },
    providers: {
        webdav: { name: 'WebDAV / Koofr', description: 'HTTPS WebDAV 폴더에 앱 비밀번호로 연결합니다.' },
        s3: { name: 'S3 호환 저장소', description: 'Amazon S3, Cloudflare R2, Backblaze B2, Hugging Face 같은 S3 호환 버킷을 씁니다.' },
        google_drive: { name: 'Google Drive', description: 'Google 계정으로 로그인해 Drive 폴더나 숨겨진 앱 데이터 공간을 씁니다.', warningTitle: '숨겨진 앱 데이터 공간에 주의하세요', warning: '숨겨진 앱 데이터 공간을 고른 경우, Drive에서 앱 데이터를 삭제하면 백업도 함께 지워집니다.' },
        onedrive: { name: 'OneDrive', description: 'Microsoft 계정으로 로그인해 개인·회사·앱 전용 폴더를 씁니다.' },
        mybox: { name: '네이버 MYBOX', description: 'MYBOX 개인 액세스 토큰과 전용 폴더를 씁니다.' },
        github_releases: { name: 'GitHub Releases', description: '비공개 저장소의 초안 릴리스에 암호화된 백업을 올립니다. 백업만 가능합니다.' },
        gitlab_packages: { name: 'GitLab 패키지', description: '전용 패키지 저장소에 암호화된 백업을 올립니다. 백업만 가능합니다.', warningTitle: 'GitLab의 정리 정책이 백업을 지울 수 있습니다', warning: '이 프로젝트에서는 패키지 정리 정책을 꺼 두세요.' },
    },
    fields: {
        'webdav.accountId': '사용자 이름', 'webdav.root': '폴더 이름', 'webdav.password': '앱 비밀번호',
        's3.bucket': '버킷', 's3.prefix': '폴더', 's3.region': '리전', 's3.addressing': '주소 방식', 's3.accessKeyId': '액세스 키 ID', 's3.secretAccessKey': '시크릿 액세스 키',
        'google_drive.folderId': '폴더 ID', 'google_drive.space': '저장 위치', 'google_drive.oauthRedirectUri': '로그인 완료 주소 (HTTPS)', 'google_drive.projectId': 'OAuth 프로젝트 ID', 'google_drive.clientId': '이 기기용 OAuth 클라이언트 ID',
        'onedrive.accountType': '계정 종류', 'onedrive.tenant': '테넌트', 'onedrive.driveId': '드라이브 ID', 'onedrive.rootItemId': '폴더 항목 ID', 'onedrive.redirectUri': '로그인 완료 주소', 'onedrive.projectId': '앱 클라이언트 ID', 'onedrive.clientId': '이 기기용 클라이언트 ID',
        'mybox.rootFolderName': '폴더 이름', 'mybox.rootFolderId': '기존 폴더 ID', 'mybox.pat': '개인 액세스 토큰', 'mybox.expiresAtMs': '토큰 만료 시각',
        'github_releases.uploadEndpoint': '업로드 주소', 'github_releases.owner': '소유자', 'github_releases.repo': '비공개 저장소 이름', 'github_releases.tagPrefix': '태그 접두어', 'github_releases.token': '개인 액세스 토큰 (fine-grained)',
        'gitlab_packages.projectId': '프로젝트 ID 또는 경로', 'gitlab_packages.packageName': '패키지 이름', 'gitlab_packages.maxFileBytes': '파일 최대 크기 (바이트)', 'gitlab_packages.token': '액세스 토큰',
    },
    fieldHelp: {
        'webdav.root': '이 폴더 안에 저장소를 만듭니다. 다른 파일과 함께 두지 마세요.',
        'webdav.password': '서비스 설정에서 만든 앱 비밀번호입니다. 계정 비밀번호가 아닙니다.',
        'github_releases.token': '백업 저장소의 콘텐츠 읽기·쓰기 권한이 필요합니다.',
        'gitlab_packages.token': 'api 범위와 이 프로젝트의 Maintainer 이상 권한이 있는 개인 또는 프로젝트 액세스 토큰을 입력하세요.',
    },
    options: {
        's3.addressing.': '서비스 기본', 's3.addressing.path': '경로 방식', 's3.addressing.virtual': '가상 호스트 방식',
        'google_drive.space.drive': '보이는 Drive 폴더', 'google_drive.space.appDataFolder': '숨겨진 앱 데이터',
        'onedrive.accountType.personal': '개인', 'onedrive.accountType.business': '회사·학교', 'onedrive.accountType.appFolder': '앱 전용 폴더',
    },
    profiles: {
        'webdav.': '일반 WebDAV', 's3.aws': 'Amazon S3', 's3.generic': '기타 S3 호환', 'gitlab_packages.': '자동', 'gitlab_packages.selfManaged': '직접 운영', 'mybox.plan': '{0} 요금제',
    },
}

export type ExternalStorageStrings = typeof english

export function externalStorageStrings(languageCode: string): ExternalStorageStrings {
    return languageCode === 'ko' ? korean : english
}

export function externalProviderName(strings: ExternalStorageStrings, providerId: ExternalProviderId): string {
    return strings.providers[providerId]?.name ?? providerId
}

export function externalFieldLabel(strings: ExternalStorageStrings, providerId: ExternalProviderId, key: string): string {
    return (strings.fields as Record<string, string>)[`${providerId}.${key}`] ?? key
}

export function externalFieldHelp(strings: ExternalStorageStrings, providerId: ExternalProviderId, key: string): string | undefined {
    return (strings.fieldHelp as Record<string, string>)[`${providerId}.${key}`]
}

export function externalOptionLabel(strings: ExternalStorageStrings, providerId: ExternalProviderId, key: string, value: string): string {
    return (strings.options as Record<string, string>)[`${providerId}.${key}.${value}`] ?? value
}

export function externalProfileLabel(strings: ExternalStorageStrings, providerId: ExternalProviderId, value: string, fallback: string): string {
    const profiles = strings.profiles as Record<string, string>
    if (providerId === 'mybox' && value.startsWith('plan')) {
        return profiles['mybox.plan'].replace('{0}', value.slice('plan'.length).toUpperCase())
    }
    return profiles[`${providerId}.${value}`] ?? fallback
}

export function externalEndpointWarning(strings: ExternalStorageStrings, code: string): string {
    return (strings.endpointWarnings as Record<string, string>)[code] ?? code
}

/** Native failure kind of a rejected command (`kind`) or of a job error DTO (`code`). */
function externalErrorKind(value: unknown): string | undefined {
    if (typeof value !== 'object' || value === null) return undefined
    const carrier = value as { kind?: unknown; code?: unknown }
    const kind = typeof carrier.kind === 'string' ? carrier.kind : carrier.code
    return typeof kind === 'string' ? kind : undefined
}

/** Sentence for a failure the native side reported. */
export function externalErrorMessage(
    strings: ExternalStorageStrings,
    value: unknown,
): string {
    const kind = externalErrorKind(value)
    switch (kind) {
        case 'alreadyConnected': return strings.connectionAlreadyAdded
        case 'unauthorized': return strings.credentialsRejected
        case 'reauthRequired': return strings.reauthenticate
        case 'notFound': return strings.repositoryNotFound
        case 'preconditionFailed': return strings.stateChanged
        case 'rateLimited':
        case 'dailyQuotaExhausted': return strings.requestBudget
        case 'storageFull': return strings.freeSpace
        case 'fileTooLarge': return strings.objectTooLarge
        case 'corrupt': return strings.corrupted
        case 'unsupported': return strings.unsupportedOperation
        case 'cancelled': return strings.interrupted
        case 'transient': return strings.retry
        default: return strings.errorGeneric
    }
}

/** Service name plus the account the native side reports, replacing the provider id in `displayName`. */
export function externalConnectionTitle(
    strings: ExternalStorageStrings,
    connection: Pick<ExternalConnectionSummary, 'providerId' | 'endpoint'>,
): string {
    const name = externalProviderName(strings, connection.providerId)
    return connection.endpoint.accountHint ? `${name} · ${connection.endpoint.accountHint}` : name
}
