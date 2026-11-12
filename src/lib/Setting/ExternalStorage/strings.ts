import { externalErrorKind } from 'src/ts/storage/sync/external/connection'
import { languageEnglish } from 'src/lang/en'
import { languageKorean } from 'src/lang/ko'
export { externalErrorKind } from 'src/ts/storage/sync/external/connection'
import type {
    ExternalConnectionSummary,
    ExternalOpenMode,
    ExternalProviderId,
} from 'src/ts/storage/sync/external/types'

const english = {
    staging: languageEnglish.risuNest.serverSync.staging,
    cameraDenied: 'Camera permission was denied. Try again or paste the connection settings.',
    cameraUnavailable: 'Could not scan the QR code. Try again or paste the connection settings.',
    changeAddress: 'Change server address', saveAddress: 'Save',
    title: 'External storage',
    help: 'Upload backups to a cloud service or your own server. All data is encrypted on this device before it is uploaded.',
    unsupported: 'External storage is available in the Android and desktop apps.',
    add: 'Connect storage', cancel: 'Cancel', refresh: 'Refresh', loading: 'Loading…', back: 'Back to the form',
    provider: 'Service', mode: 'Repository', create: 'Create new', existing: 'Connect existing repository',
    existingHelp: 'Connecting an existing repository needs the recovery key of that repository.',
    purpose: 'Purpose', backup: 'Backup only', sync: 'Synchronization',
    requestUsage: 'Estimated requests used on this device',
    connectionInfo: 'Connection details', endpoint: 'Server address', profile: 'Service type', optional: ' (optional)',
    library: 'Characters, chats and attachments',
    hypa: 'Hypa embedding data', deviceSettings: 'Local settings', devicePlugins: 'Local plugin data',
    prepare: 'Review details',
    pendingVerification: 'The next step reviews what you entered and asks you to sign in or to enter the password or key.',
    githubWarningTitle: 'Create a separate private repository for backups',
    githubWarning: 'This connection keeps creating draft releases and tags. Do not connect a repository you use for anything else.',
    acknowledge: 'I understand',
    endpointReview: 'Where this connects', authority: 'Server', account: 'Account', repository: 'Repository location', purposeReview: 'Purpose', includes: 'includes {0}',
    confirmEndpoint: 'This server and folder are correct', connect: 'Connect', signIn: 'Sign in', finishSignIn: 'Finish sign-in',
    folder: 'Folder', selectFolder: 'Select folder', selectFolderAgain: 'Select again', selectingFolder: 'Selecting folder',
    selectThisFolder: 'Select this folder', currentFolder: 'Current folder', close: 'Close',
    noSubfolders: 'This folder has no subfolders.',
    folderSelectHelp: 'The repository folder is selected after signing in, in the next step.',
    folderNameRequired: 'Enter a folder name.',
    fieldRequired: 'Fill in this field.',
    folderNameConflict: 'A folder with this name already exists. Enter another name.',
    folderCreateFailed: 'Could not create the folder. Try again.',
    folderInaccessible: 'The selected folder cannot be opened. Select the folder again.',
    folderNotRepository: 'The selected folder holds no RisuNest repository.',
    folderUnsupportedLocation: 'This location is not supported. Select a folder in My Drive.',
    connectHint: 'Connect contacts the server, creates the folder and then shows your recovery key.',
    recoveryCode: 'Recovery key',
    connectionSettings: 'Connection settings',
    connectionSettingsImportHelp: 'Import encrypted connection settings, or enter the provider details below. The recovery key and provider access are still required.',
    openConnectionSettingsFile: 'Open settings file', scanConnectionSettings: 'Scan settings QR',
    connectionSettingsPayload: 'Connection settings', connectionSettingsPayloadPlaceholder: 'Paste connection settings, open a settings file or scan its QR code', importConnectionSettings: 'Import settings',
    manualConnectionHelp: 'To connect without a settings file, enter the provider details and recovery key.',
    noConnections: 'No external storage is connected. Press Connect storage to connect WebDAV, an S3-compatible bucket, Google Drive, OneDrive, NAVER MYBOX, GitHub or GitLab.',
    makeSyncTarget: 'Sync with this repository',
    runBackup: 'Back up now', runSync: 'Sync now', history: 'History', quota: 'Storage usage', recovery: 'Recovery key', remove: 'Disconnect',
    restore: 'Restore', download: 'Save as file', pin: 'Keep from cleanup', pinned: 'Kept', deleteHistory: 'Delete', local: 'Keep this device', remote: 'Keep repository',
    thisDevice: 'This device',
    noHistory: 'This repository holds no backup points yet.',
    noSyncHistory: 'This repository holds no history yet.',
    deleteHistoryConfirm: 'Delete this backup point from the repository?',
    deleteHistoryAcknowledge: 'Permanently delete this backup point from the repository',
    deleteOtherDeviceConfirm: 'This backup was made on another device.',
    deleteLastRetainedConfirm: 'This is the last retained backup point.',
    lastBackup: 'Last backup', progress: 'Progress', remaining: 'Remaining',
    saveConnectionSettings: 'Save settings file', connectionSettingsQr: 'Connection settings QR code', closeRecovery: 'Close',
    recoveryCodeHelp: 'Save this key in a secure place. It cannot be replaced or recovered by RisuNest.',
    connectionSettingsNotice: 'These encrypted settings help another device reach this repository. The recovery key and provider access are still required.',
    connectionSettingsFileOnly: 'The QR code cannot be displayed. Copy the connection settings or save the settings file.',
    copyConnectionSettings: 'Copy', connectionSettingsCopied: 'Connection settings copied.',
    connectionSettingsCopyFailed: 'Could not copy. Select and copy the text.',
    platformClientId: 'OAuth client ID for this device', oauthProjectHint: 'OAuth project',
    authorizationUnavailable: 'Signing in needs a native iOS callback, which is not available in this build.',
    oneDriveAndroidSetup: 'On Android, add risunestlocal://oauth/onedrive under Mobile and desktop applications and enable public client flows in the Entra app.',
    googleAndroidSetup: 'In Google Cloud, create a Web OAuth client, add the exact HTTPS callback address below, and enable the Google Drive API. After signing in, the callback page returns to RisuNest by itself.',
    googleIOSSetup: 'In Google Cloud, create an iOS OAuth client with bundle ID io.github.rsyumi.risunest and enable the Google Drive API.',
    oneDriveIOSSetup: 'In the Entra app, add an iOS/macOS platform with bundle ID io.github.rsyumi.risunest and enable public client flows.',
    iosOAuthClientId: 'iOS OAuth client ID',
    webOAuthClientId: 'Web OAuth client ID', oauthCallbackUrl: 'Sign-in completion address (HTTPS)', oauthClientSecret: 'Client secret',
    manualOAuthCallback: 'Sign-in completion page address (if the app does not return by itself)', manualOAuthHelp: 'If the app does not reopen by itself, paste the complete address of the sign-in completion page.',
    renewalClientSecretHelp: 'Leave blank to use the client secret saved with this connection.',
    authorizationWaiting: 'Finish signing in in the browser, then press Finish sign-in.',
    authorizationWaitingMobile: 'After signing in in the browser, you will return to the app.',
    callbackRejected: 'That address does not belong to this sign-in attempt. Check it and paste the complete address again.',
    oauthErrorCode: 'OAuth error code: {0}',
    oauthErrorDescription: 'OAuth error description: {0}',
    recoveryNotice: 'Save this recovery key now. It stays fixed for this repository and cannot be shown or replaced later. If it is lost, create and verify a new repository before deleting this one.',
    recheckPublication: 'Check publication again',
    restoreUnfinished: 'The external storage restore did not finish. Restore the same backup again to continue.',
    stopRestoreTitle: 'Stop the restore?',
    stopRestoreDescription: 'The restore stops and the current data stays as it is. Files not yet downloaded may be unavailable.',
    stopRestore: 'Stop',
    publicationDecision: 'The result of this publication could not be confirmed. Automatic publication has stopped. Check again, pause automatic sync, or select another sync target.',
    cancelled: 'Cancelled', retryAt: 'Try again after {0}.', automaticBackup: 'Automatic backup', renew: 'Sign in again', unlock: 'Unlock',
    completed: 'Completed', failed: 'Failed', queued: 'Queued', running: 'Working…', waiting: 'Waiting', uncertain: 'Could not confirm that the repository was updated. Run the same action again to check.',
    switchSyncTitle: 'Sync with this repository?',
    switchSyncFromServer: 'This device stops syncing with the sync server and syncs with this repository.',
    switchSyncFromExternal: 'This device stops syncing with the other external storage and syncs with this repository.',
    startSync: 'Sync', statusPaused: 'Paused',
    loadMore: 'Show older history', statusReady: 'Connected', statusReauth: 'Sign-in required', statusLocked: 'Recovery key required', statusError: 'Needs attention',
    usedByService: 'Used on the service', unknownUsage: 'Unknown', uploadedLowerBound: 'Repository data known to this device', lastUsageUpdate: 'Last checked', latestReachable: 'Size of the latest known backup', atLeast: '{0} or more', files: '{0} files', remoteOnlyFiles: 'Files only in this external storage',
    transferConcurrency: 'Concurrent transfers',
    transferConcurrencyHelp: 'Set the maximum number of simultaneous uploads and downloads. Lower the value if errors recur.',
    cleanup: 'Clean up now', retentionCount: 'Backups to keep', retentionDays: 'Keep for', retentionDaysUnit: 'days',
    retentionHelp: 'Cleanup removes automatic backups made on this device once they are past both the number of backups to keep and the days to keep them.',
    providerCapacityHelp: 'File versions, trash and unfinished uploads may continue to use space on the service. Remove them through the service when needed.',
    check: 'Check',
    checkSummary: 'Verified {0} files ({1}).',
    checkDamaged: 'Could not read {0} of them, so it cannot be used to restore.',
    checkExpired: 'The repository changed while this check was stopped, so it cannot finish. Check again.',
    cleanupSummary: 'Removed {0} files ({1}).',
    cleanupPartial: 'Some files are left for the next cleanup.',
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
    stateChanged: 'Data changed while this was running. Try again.',
    requestBudget: 'The service request limit was reached. Try again later.',
    objectTooLarge: 'A file is larger than this service allows.',
    corrupted: 'The stored data did not pass verification. Choose another backup or connect the repository again.',
    recoveryKeyMismatch: 'The recovery key does not match this repository. Check the key.',
    repositoryMismatch: 'This location holds a different repository. Check the folder, bucket and address.',
    formatSwitchRequired: languageEnglish.lwwSync.formatSwitchRequiredHelp,
    appUpdateRequired: languageEnglish.lwwSync.appUpdateRequiredHelp,
    unsupportedOperation: 'This repository cannot do that.',
    requestLengthRequired: 'The server rejected the request.',
    interrupted: 'This stopped before it finished. Try again.',
    endpointRejected: 'The server address was rejected. Check the address.',
    deviceVaultUnavailable: 'Unlock this device or its keyring, then try again.',
    clockSkew: 'Correct this device’s date and time, then try again.',
    folderTooLarge: 'This folder contains too many entries. Select another folder.',
    repositoryBusy: 'Another storage operation is running. Try again after it finishes.',
    locationOccupied: 'This location is already in use. Select another location.',
    authorizationTimedOut: 'Sign-in timed out. Sign in again.',
    localStorageFull: 'This device is out of space. Free space and try again.',
    localPermissionDenied: 'The app cannot access a local file. Check its permissions and try again.',
    localFailure: 'This device could not complete this. Try again.',
    errorGeneric: 'Could not complete this. Try again.',
    removeTitle: 'Disconnect this external storage?',
    removeAcknowledge: 'Delete the connection details and recovery key on this device',
    removeDeletes: 'The connection details and recovery key on this device are deleted.',
    removeRemoteOnly: 'Some files are stored only in this external storage. If you need them, download them before disconnecting.',
    removeRemoteOnlyUnknown: 'Files could not be checked. Download any files stored only in this external storage before disconnecting if you need them.',
    downloadThenRemove: 'Download, then disconnect',
    downloadFailedKeptConnection: 'The files could not be downloaded, so the connection was kept. Try again, or disconnect without downloading.',
    jobKinds: { cleanup: 'Cleanup', backup: 'Backup', restore: 'Restore', 'pin-history': 'Keep', 'delete-history': 'Delete backup', 'check-repository': 'Check' },
    jobActive: { cleanup: 'Cleaning up', backup: 'Backing up', restore: 'Restoring', 'pin-history': 'Keeping', 'delete-history': 'Deleting backup', 'check-repository': 'Checking' },
    restorePhases: { downloading: 'Downloading backup data', 'preparing-local': 'Preparing the restore', 'applying-local': 'Applying the restore', 'awaiting-adoption': 'Applying the restore', 'receiving-assets': 'Downloading assets' } as Record<string, string>,
    jobCounters: { prepared: 'Prepared', transferred: 'Transferred' },
    transfer: {
        uploadSpeed: 'Upload speed', downloadSpeed: 'Download speed',
        details: 'Details', syncing: 'Syncing', connecting: 'Connecting', downloading: 'Downloading sync data', filesDownloading: 'Downloading files', waiting: 'Waiting for your response', syncComplete: 'Sync completed', downloadComplete: 'Download completed', connectionComplete: 'Connected',
        prepared: 'Prepared', uploaded: 'Uploaded', downloaded: 'Downloaded', objects: '{0} items', files: 'Files ready',
        checking: 'Checking repository', preparing: 'Preparing data', uploading: 'Uploading', applying: 'Applying data', finalizing: 'Finishing',
    },
    historyKinds: { snapshot: 'Sync', 'backup-point': 'Backup', conflict: 'Conflict backup', 'recovery-candidate': 'Recovery candidate' },
    endpointWarnings: {
        'github-dedicated-repository': 'Use a separate private repository. This connection is backup only.',
        'gitlab-cleanup-policy': 'GitLab package cleanup policies can delete backups. Keep cleanup policies off for this project. This connection is backup only.',
    },
    providers: {
        webdav: { name: 'WebDAV', description: 'Connects to a WebDAV folder. Leave the account and password empty if authentication is not required.' },
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
        'google_drive.folderName': 'Folder name', 'google_drive.space': 'Storage location', 'google_drive.oauthRedirectUri': 'Sign-in completion address (HTTPS)', 'google_drive.projectId': 'OAuth project ID', 'google_drive.clientId': 'OAuth client ID for this device',
        'onedrive.accountType': 'Account type', 'onedrive.folderName': 'Folder name', 'onedrive.tenant': 'Tenant', 'onedrive.redirectUri': 'Sign-in completion address', 'onedrive.projectId': 'App client ID', 'onedrive.clientId': 'Client ID for this device',
        'mybox.rootFolderName': 'Folder name', 'mybox.rootFolderId': 'Existing folder ID', 'mybox.pat': 'Personal access token', 'mybox.expiresAtMs': 'Token expiry',
        'github_releases.uploadEndpoint': 'Upload address', 'github_releases.owner': 'Owner', 'github_releases.repo': 'Private repository name', 'github_releases.tagPrefix': 'Tag prefix', 'github_releases.token': 'Personal access token (fine-grained)',
        'gitlab_packages.projectId': 'Project ID or path', 'gitlab_packages.packageName': 'Package name', 'gitlab_packages.maxFileBytes': 'Maximum file size (bytes)', 'gitlab_packages.token': 'Access token',
    },
    fieldHelp: {
        'google_drive.folderName': 'After you sign in, a new folder with this name is created and the repository is created inside it.',
        'onedrive.folderName': 'After you sign in, a new folder with this name is created and the repository is created inside it.',
        'webdav.root': 'The repository is created inside this folder. Do not keep other files in it.',
        'webdav.password': 'An application password created in the service settings, not your account password.',
        'github_releases.token': 'Needs read and write access to the contents of the backup repository.',
        'gitlab_packages.token': 'Use a personal or project access token with api scope and Maintainer or Owner access to this project.',
    },
    existingFieldHelp: {
        'webdav.root': 'The folder that holds the repository.',
    },
    options: {
        's3.addressing.': 'Service default', 's3.addressing.path': 'Path style', 's3.addressing.virtual': 'Virtual host style',
        'google_drive.space.drive': 'Visible Drive folder', 'google_drive.space.appDataFolder': 'Hidden app data',
        'onedrive.accountType.personal': 'Personal', 'onedrive.accountType.business': 'Work or school', 'onedrive.accountType.appFolder': 'App-only folder',
    },
    profiles: {
        'webdav.': 'Generic WebDAV', 's3.aws': 'Amazon S3', 's3.generic': 'Other S3-compatible', 'gitlab_packages.': 'Automatic', 'gitlab_packages.selfManaged': 'Self-managed', 'mybox.plan': '{0} plan',
    },

    quotaBuckets: {
        'mybox-download-day': 'Downloads per day',
        'mybox-download-url-minute': 'Downloads per minute',
        'mybox-upload-url-minute': 'Uploads per minute',
        'mybox-metadata-minute': 'File info lookups per minute',
        'mybox-list-minute': 'Listings per minute',
        'mybox-folder-minute': 'Folders created per minute',
        'mybox-delete-minute': 'Deletions per minute',
    } as Record<string, string>,
    requestCount: '{0} / {1} requests',
}

const korean: typeof english = {
    cameraDenied: '카메라 권한이 허용되지 않았습니다. 다시 시도하거나 연결 설정을 붙여넣어 주세요.',
    cameraUnavailable: '카메라로 QR 코드를 읽지 못했습니다. 다시 시도하거나 연결 설정을 붙여넣어 주세요.',
    changeAddress: '서버 주소 변경', saveAddress: '저장',
    staging: languageKorean.risuNest.serverSync.staging,
    title: '외부 저장소',
    help: '클라우드나 개인 서버에 백업을 올릴 수 있습니다. 모든 데이터는 업로드하기 전 이 기기에서 암호화됩니다.',
    unsupported: '외부 저장소는 Android 및 데스크톱 앱에서 사용할 수 있습니다.',
    add: '저장소 연결', cancel: '취소', refresh: '새로 고침', loading: '불러오는 중…', back: '입력으로 돌아가기',
    provider: '서비스', mode: '저장소', create: '새로 만들기', existing: '이미 있는 저장소 연결',
    existingHelp: '이미 있는 저장소를 연결할 때는 그 저장소의 복구 키가 필요합니다.',
    purpose: '용도', backup: '백업만', sync: '동기화',
    requestUsage: '이 기기의 예상 요청 사용량',
    connectionInfo: '연결 정보', endpoint: '서버 주소', profile: '서비스 종류', optional: ' (선택)',
    library: '캐릭터·대화와 첨부 파일',
    hypa: '하이파 임베딩 데이터', deviceSettings: '로컬 설정', devicePlugins: '로컬 플러그인 데이터',
    prepare: '입력 내용 확인',
    pendingVerification: '다음 단계에서 입력한 내용을 검토하고 로그인을 하거나, 비밀번호/키를 입력하게 됩니다.',
    githubWarningTitle: '백업 전용 비공개 저장소를 따로 만드세요',
    githubWarning: '이 연결은 초안 릴리스와 태그를 계속 만듭니다. 다른 용도로 쓰는 저장소에는 연결하지 마세요.',
    acknowledge: '확인했습니다',
    endpointReview: '연결할 곳', authority: '서버', account: '계정', repository: '저장소 위치', purposeReview: '용도', includes: '{0} 포함',
    confirmEndpoint: '이 서버와 폴더가 맞습니다', connect: '연결', signIn: '로그인', finishSignIn: '로그인 완료',
    folder: '폴더', selectFolder: '폴더 선택', selectFolderAgain: '다시 선택', selectingFolder: '폴더 선택 중',
    selectThisFolder: '이 폴더 선택', currentFolder: '현재 폴더', close: '닫기',
    noSubfolders: '하위 폴더가 없습니다.',
    folderSelectHelp: '저장소 폴더는 다음 단계에서 로그인한 뒤 선택합니다.',
    folderNameRequired: '폴더 이름을 입력하세요.',
    fieldRequired: '이 항목을 입력하세요.',
    folderNameConflict: '같은 이름의 폴더가 있습니다. 다른 이름을 입력하세요.',
    folderCreateFailed: '폴더를 만들지 못했습니다. 다시 시도하세요.',
    folderInaccessible: '선택한 폴더에 접근할 수 없습니다. 폴더를 다시 선택하세요.',
    folderNotRepository: '선택한 폴더에 RisuNest 저장소가 없습니다.',
    folderUnsupportedLocation: '이 위치는 지원되지 않습니다. 내 드라이브의 폴더를 선택하세요.',
    connectHint: '연결을 누르면 서버에 접속해 폴더를 만들고 복구 키를 보여드립니다.',
    recoveryCode: '복구 키',
    connectionSettings: '연결 설정',
    connectionSettingsImportHelp: '암호화된 연결 설정을 가져오거나 아래에 서비스 연결 정보를 입력하세요. 복구 키와 서비스 접근 권한은 계속 필요합니다.',
    openConnectionSettingsFile: '설정 파일 열기', scanConnectionSettings: '설정 QR 스캔',
    connectionSettingsPayload: '연결 설정', connectionSettingsPayloadPlaceholder: '연결 설정을 붙여넣거나 설정 파일을 열거나 QR 코드를 스캔하세요', importConnectionSettings: '설정 가져오기',
    manualConnectionHelp: '설정 파일 없이 연결하려면 서비스 연결 정보와 복구 키를 입력하세요.',
    noConnections: '연결된 외부 저장소가 없습니다. 저장소 연결을 눌러 WebDAV, S3 호환 저장소, Google Drive, OneDrive, 네이버 MYBOX, GitHub, GitLab 중 하나를 연결하세요.',
    makeSyncTarget: '이 저장소로 동기화',
    runBackup: '지금 백업', runSync: '지금 동기화', history: '이력', quota: '저장소 용량', recovery: '복구 키', remove: '연결 해제',
    restore: '복원', download: '파일로 저장', pin: '지우지 않고 보관', pinned: '보관 중', deleteHistory: '삭제', local: '이 기기 내용 유지', remote: '저장소 내용 유지',
    thisDevice: '이 기기',
    noHistory: '이 저장소에 백업 지점이 아직 없습니다.',
    noSyncHistory: '이 저장소에 이력이 아직 없습니다.',
    deleteHistoryConfirm: '이 백업 지점을 저장소에서 삭제하시겠습니까?',
    deleteHistoryAcknowledge: '저장소에서 이 백업 지점 영구 삭제',
    deleteOtherDeviceConfirm: '다른 기기에서 만든 백업입니다.',
    deleteLastRetainedConfirm: '마지막으로 보관된 백업 지점입니다.',
    lastBackup: '마지막 백업', progress: '진행', remaining: '남은 시간',
    saveConnectionSettings: '설정 파일 저장', connectionSettingsQr: '연결 설정 QR 코드', closeRecovery: '닫기',
    recoveryCodeHelp: '이 키를 안전한 곳에 보관하세요. RisuNest에서 교체하거나 복구할 수 없습니다.',
    connectionSettingsNotice: '다른 기기에서 이 저장소에 접근할 때 쓰는 암호화된 설정입니다. 복구 키와 서비스 접근 권한은 계속 필요합니다.',
    connectionSettingsFileOnly: 'QR 코드를 표시할 수 없습니다. 연결 설정을 복사하거나 설정 파일을 저장하세요.',
    copyConnectionSettings: '복사', connectionSettingsCopied: '연결 설정을 복사했습니다.',
    connectionSettingsCopyFailed: '복사하지 못했습니다. 내용을 선택해 복사하세요.',
    platformClientId: '이 기기용 OAuth 클라이언트 ID', oauthProjectHint: 'OAuth 프로젝트',
    authorizationUnavailable: '로그인에는 네이티브 iOS 콜백이 필요하지만 이 빌드에서는 사용할 수 없습니다.',
    oneDriveAndroidSetup: 'Android에서는 Entra 앱의 모바일 및 데스크톱 애플리케이션에 risunestlocal://oauth/onedrive를 추가하고 공용 클라이언트 흐름을 사용 설정하세요.',
    googleAndroidSetup: 'Google Cloud에서 웹 OAuth 클라이언트를 만들고 아래의 HTTPS 주소를 그대로 추가한 뒤 Google Drive API를 사용 설정하세요. 로그인이 끝나면 완료 페이지가 RisuNest로 자동으로 돌아옵니다.',
    googleIOSSetup: 'Google Cloud에서 번들 ID가 io.github.rsyumi.risunest인 iOS OAuth 클라이언트를 만들고 Google Drive API를 사용 설정하세요.',
    oneDriveIOSSetup: 'Entra 앱의 iOS/macOS 플랫폼에 번들 ID io.github.rsyumi.risunest를 추가하고 공용 클라이언트 흐름을 사용 설정하세요.',
    iosOAuthClientId: 'iOS OAuth 클라이언트 ID',
    webOAuthClientId: '웹 OAuth 클라이언트 ID', oauthCallbackUrl: '로그인 완료 주소 (HTTPS)', oauthClientSecret: '클라이언트 보안 비밀',
    manualOAuthCallback: '로그인 완료 페이지 주소 (자동으로 돌아오지 않을 때)', manualOAuthHelp: '앱이 자동으로 다시 열리지 않으면 로그인 완료 페이지의 주소를 통째로 붙여넣으세요.',
    renewalClientSecretHelp: '비워 둘 경우 이 연결에 저장된 클라이언트 보안 비밀을 사용합니다.',
    authorizationWaiting: '브라우저에서 로그인을 마친 뒤 로그인 완료를 누르세요.',
    authorizationWaitingMobile: '브라우저에서 로그인을 마치면 앱으로 돌아옵니다.',
    callbackRejected: '이 주소는 현재 로그인 시도의 것이 아닙니다. 확인한 뒤 주소를 통째로 다시 붙여넣으세요.',
    oauthErrorCode: 'OAuth 오류 코드: {0}',
    oauthErrorDescription: 'OAuth 오류 설명: {0}',
    recoveryNotice: '이 복구 키를 지금 안전한 곳에 보관하세요. 저장소에서 계속 같은 키를 사용하며, 나중에 다시 표시하거나 교체할 수 없습니다. 키를 잃은 경우 새 저장소에 백업하고 검증한 뒤 기존 저장소를 삭제하세요.',
    recheckPublication: '반영 결과 다시 확인',
    restoreUnfinished: '외부 저장소 복원이 완료되지 않았습니다. 같은 백업을 다시 복원하면 이어서 진행합니다.',
    stopRestoreTitle: '복원을 중단하시겠습니까?',
    stopRestoreDescription: '복원을 중단하고 현재 데이터를 그대로 둡니다. 아직 받지 못한 파일은 사용할 수 없을 수 있습니다.',
    stopRestore: '중단',
    publicationDecision: '저장소 반영 결과를 확인하지 못했으며, 자동 반영이 중지되었습니다. 다시 확인하거나 자동 동기화를 중지하고 동기화 대상을 변경할 수 있습니다.',
    cancelled: '취소됨', retryAt: '{0} 이후 다시 시도하세요.', automaticBackup: '자동 백업', renew: '다시 로그인', unlock: '잠금 해제',
    completed: '완료', failed: '실패', queued: '대기열에 추가됨', running: '처리 중…', waiting: '대기 중', uncertain: '저장소에 반영됐는지 확인하지 못했습니다. 다시 확인하려면 같은 작업을 다시 실행하세요.',
    switchSyncTitle: '이 저장소로 동기화하시겠습니까?',
    switchSyncFromServer: '동기화 서버와의 동기화를 중지하고 이 저장소와 동기화합니다.',
    switchSyncFromExternal: '다른 외부 저장소와의 동기화를 중지하고 이 저장소와 동기화합니다.',
    startSync: '동기화', statusPaused: '일시 중지됨',
    loadMore: '이전 이력 더 보기', statusReady: '연결됨', statusReauth: '다시 로그인 필요', statusLocked: '복구 키 필요', statusError: '확인 필요',
    usedByService: '서비스에서 쓰는 용량', unknownUsage: '알 수 없음', uploadedLowerBound: '이 기기에서 확인한 저장소 데이터', lastUsageUpdate: '마지막 확인', latestReachable: '확인한 최신 백업 크기', atLeast: '{0} 이상', files: '파일 {0}개', remoteOnlyFiles: '이 외부 저장소에만 있는 파일',
    transferConcurrency: '동시 전송 수',
    transferConcurrencyHelp: '동시에 진행할 업로드와 다운로드 수를 설정합니다. 오류가 반복될 경우 값을 낮춰주세요.',
    cleanup: '지금 정리', retentionCount: '보관 개수', retentionDays: '보관 기간', retentionDaysUnit: '일',
    retentionHelp: '정리할 때 이 기기에서 만든 자동 백업 중 보관 개수와 보관 기간을 모두 넘긴 백업을 지웁니다.',
    providerCapacityHelp: '서비스의 파일 버전, 휴지통, 완료되지 않은 업로드가 용량을 계속 차지할 수 있습니다. 필요한 경우 서비스에서 정리하세요.',
    check: '검증',
    checkSummary: '{0}개를 검증했습니다 ({1}).',
    checkDamaged: '이 중 {0}개를 읽지 못했으며, 복원에 쓸 수 없습니다.',
    checkExpired: '검증이 멈춘 사이에 저장소가 바뀌어 이 검증을 마칠 수 없습니다. 다시 검증하세요.',
    cleanupSummary: '{0}개를 지웠습니다 ({1}).',
    cleanupPartial: '남은 파일은 다음 정리에서 지웁니다.',
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
    stateChanged: '작업하는 사이에 데이터가 바뀌었습니다. 다시 실행하세요.',
    requestBudget: '서비스의 요청 한도에 걸렸습니다. 잠시 뒤 다시 시도하세요.',
    objectTooLarge: '이 서비스가 허용하는 크기보다 큰 파일이 있습니다.',
    corrupted: '저장된 데이터가 검증을 통과하지 못했습니다. 다른 백업을 고르거나 저장소를 다시 연결하세요.',
    recoveryKeyMismatch: '복구 키가 이 저장소와 맞지 않습니다. 복구 키를 확인하세요.',
    repositoryMismatch: '이 위치에 다른 저장소가 있습니다. 폴더·버킷과 주소를 확인하세요.',
    formatSwitchRequired: languageKorean.lwwSync.formatSwitchRequiredHelp,
    appUpdateRequired: languageKorean.lwwSync.appUpdateRequiredHelp,
    unsupportedOperation: '이 저장소에서는 할 수 없는 작업입니다.',
    requestLengthRequired: '서버가 요청을 거부했습니다.',
    interrupted: '끝나기 전에 멈췄습니다. 다시 시도하세요.',
    endpointRejected: '서버 주소를 사용할 수 없습니다. 주소를 확인하세요.',
    deviceVaultUnavailable: '기기 또는 키링의 잠금을 해제한 뒤 다시 시도하세요.',
    clockSkew: '기기의 날짜와 시간을 확인한 뒤 다시 시도하세요.',
    folderTooLarge: '폴더에 항목이 너무 많습니다. 다른 폴더를 선택하세요.',
    repositoryBusy: '다른 저장소 작업이 진행 중입니다. 완료된 뒤 다시 시도하세요.',
    locationOccupied: '이미 사용 중인 위치입니다. 다른 위치를 선택하세요.',
    authorizationTimedOut: '로그인 시간이 초과되었습니다. 다시 로그인하세요.',
    localStorageFull: '기기 공간이 부족합니다. 공간을 확보한 뒤 다시 시도하세요.',
    localPermissionDenied: '로컬 파일에 접근할 수 없습니다. 권한을 확인한 뒤 다시 시도하세요.',
    localFailure: '이 기기에서 작업을 마치지 못했습니다. 다시 시도하세요.',
    errorGeneric: '작업을 마치지 못했습니다. 다시 시도하세요.',
    removeTitle: '연결을 해제하시겠습니까?',
    removeAcknowledge: '이 기기의 연결 정보와 복구 키 삭제',
    removeDeletes: '이 기기의 연결 정보와 복구 키가 삭제됩니다.',
    removeRemoteOnly: '이 외부 저장소에만 있는 파일이 있습니다. 필요한 경우 다운로드한 뒤 연결을 해제하세요.',
    removeRemoteOnlyUnknown: '파일을 확인할 수 없습니다. 필요한 경우 이 외부 저장소에만 있는 파일을 다운로드한 뒤 연결을 해제하세요.',
    downloadThenRemove: '다운로드 후 연결 해제',
    downloadFailedKeptConnection: '파일을 다운로드하지 못해 연결을 해제하지 않았습니다. 다시 시도하거나 다운로드하지 않고 연결을 해제하세요.',
    jobKinds: { cleanup: '정리', backup: '백업', restore: '복원', 'pin-history': '보관', 'delete-history': '백업 삭제', 'check-repository': '검증' },
    jobActive: { cleanup: '정리 중', backup: '백업 중', restore: '복원 중', 'pin-history': '보관 중', 'delete-history': '백업 삭제 중', 'check-repository': '검증 중' },
    restorePhases: { downloading: '백업 데이터 받는 중', 'preparing-local': '복원 준비 중', 'applying-local': '복원 적용 중', 'awaiting-adoption': '복원 적용 중', 'receiving-assets': '에셋 받는 중' },
    jobCounters: { prepared: '준비', transferred: '전송' },
    transfer: {
        uploadSpeed: '업로드 속도', downloadSpeed: '다운로드 속도',
        details: '자세히', syncing: '동기화 중', connecting: '연결 중', downloading: '동기화 데이터 다운로드 중', filesDownloading: '파일 다운로드 중', waiting: '사용자 응답 대기 중', syncComplete: '동기화 완료', downloadComplete: '다운로드 완료', connectionComplete: '연결 완료',
        prepared: '준비', uploaded: '업로드 완료', downloaded: '다운로드 완료', objects: '{0}개 항목', files: '준비된 파일',
        checking: '저장소 확인 중', preparing: '데이터 준비 중', uploading: '업로드 중', applying: '데이터 반영 중', finalizing: '마무리 중',
    },
    historyKinds: { snapshot: '동기화', 'backup-point': '백업', conflict: '충돌 백업', 'recovery-candidate': '복구 후보' },
    endpointWarnings: {
        'github-dedicated-repository': '백업 전용 비공개 저장소를 따로 쓰세요. 이 연결은 백업만 합니다.',
        'gitlab-cleanup-policy': 'GitLab의 패키지 정리 정책이 백업을 지울 수 있습니다. 이 프로젝트에서는 정리 정책을 꺼 두세요. 이 연결은 백업만 합니다.',
    },
    providers: {
        webdav: { name: 'WebDAV', description: 'WebDAV 폴더에 연결합니다. 인증이 필요하지 않은 경우 계정과 비밀번호를 비워두세요.' },
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
        'google_drive.folderName': '폴더 이름', 'google_drive.space': '저장 위치', 'google_drive.oauthRedirectUri': '로그인 완료 주소 (HTTPS)', 'google_drive.projectId': 'OAuth 프로젝트 ID', 'google_drive.clientId': '이 기기용 OAuth 클라이언트 ID',
        'onedrive.accountType': '계정 종류', 'onedrive.folderName': '폴더 이름', 'onedrive.tenant': '테넌트', 'onedrive.redirectUri': '로그인 완료 주소', 'onedrive.projectId': '앱 클라이언트 ID', 'onedrive.clientId': '이 기기용 클라이언트 ID',
        'mybox.rootFolderName': '폴더 이름', 'mybox.rootFolderId': '기존 폴더 ID', 'mybox.pat': '개인 액세스 토큰', 'mybox.expiresAtMs': '토큰 만료 시각',
        'github_releases.uploadEndpoint': '업로드 주소', 'github_releases.owner': '소유자', 'github_releases.repo': '비공개 저장소 이름', 'github_releases.tagPrefix': '태그 접두어', 'github_releases.token': '개인 액세스 토큰 (fine-grained)',
        'gitlab_packages.projectId': '프로젝트 ID 또는 경로', 'gitlab_packages.packageName': '패키지 이름', 'gitlab_packages.maxFileBytes': '파일 최대 크기 (바이트)', 'gitlab_packages.token': '액세스 토큰',
    },
    fieldHelp: {
        'google_drive.folderName': '로그인한 뒤 이 이름의 새 폴더를 만들고 그 안에 저장소를 만듭니다.',
        'onedrive.folderName': '로그인한 뒤 이 이름의 새 폴더를 만들고 그 안에 저장소를 만듭니다.',
        'webdav.root': '이 폴더 안에 저장소를 만듭니다. 다른 파일과 함께 두지 마세요.',
        'webdav.password': '서비스 설정에서 만든 앱 비밀번호입니다. 계정 비밀번호가 아닙니다.',
        'github_releases.token': '백업 저장소의 콘텐츠 읽기·쓰기 권한이 필요합니다.',
        'gitlab_packages.token': 'api 범위와 이 프로젝트의 Maintainer 이상 권한이 있는 개인 또는 프로젝트 액세스 토큰을 입력하세요.',
    },
    existingFieldHelp: {
        'webdav.root': '저장소가 있는 폴더입니다.',
    },
    options: {
        's3.addressing.': '서비스 기본', 's3.addressing.path': '경로 방식', 's3.addressing.virtual': '가상 호스트 방식',
        'google_drive.space.drive': '보이는 Drive 폴더', 'google_drive.space.appDataFolder': '숨겨진 앱 데이터',
        'onedrive.accountType.personal': '개인', 'onedrive.accountType.business': '회사·학교', 'onedrive.accountType.appFolder': '앱 전용 폴더',
    },
    profiles: {
        'webdav.': '일반 WebDAV', 's3.aws': 'Amazon S3', 's3.generic': '기타 S3 호환', 'gitlab_packages.': '자동', 'gitlab_packages.selfManaged': '직접 운영', 'mybox.plan': '{0} 요금제',
    },

    quotaBuckets: {
        'mybox-download-day': '하루 다운로드',
        'mybox-download-url-minute': '분당 다운로드',
        'mybox-upload-url-minute': '분당 업로드',
        'mybox-metadata-minute': '분당 파일 정보 조회',
        'mybox-list-minute': '분당 목록 조회',
        'mybox-folder-minute': '분당 폴더 생성',
        'mybox-delete-minute': '분당 삭제',
    },
    requestCount: '{0} / {1}회',
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

export function externalFieldHelp(
    strings: ExternalStorageStrings,
    providerId: ExternalProviderId,
    key: string,
    mode: ExternalOpenMode = 'create',
): string | undefined {
    const field = `${providerId}.${key}`
    return (mode === 'existing' ? (strings.existingFieldHelp as Record<string, string>)[field] : undefined)
        ?? (strings.fieldHelp as Record<string, string>)[field]
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

const folderErrorKinds = ['folderInaccessible', 'folderNotRepository', 'folderUnsupportedLocation'] as const

/** Whether a native failure concerns the selected folder rather than the connection as a whole. */
export function externalFolderErrorKind(value: unknown): boolean {
    const kind = externalErrorKind(value)
    return kind !== undefined && (folderErrorKinds as readonly string[]).includes(kind)
}

/** Sentence for a failure the native side reported. */
export function externalErrorMessage(
    strings: ExternalStorageStrings,
    value: unknown,
): string {
    const code = value instanceof Error ? value.message : value
    if (code === 'qr-camera-permission-denied' || code === 'qr-camera-permission-blocked') return strings.cameraDenied
    if (code === 'qr-camera-unavailable' || code === 'qr-scan-timeout' || code === 'qr-scan-busy') return strings.cameraUnavailable
    const summary = externalErrorSummary(strings, value)
    const details: string[] = []
    if (typeof value === 'string' && value.trim()) details.push(value)
    if (typeof value === 'object' && value !== null) {
        const error = value as { httpStatus?: unknown; detail?: unknown; message?: unknown }
        if (typeof error.httpStatus === 'number') details.push(`HTTP ${error.httpStatus}`)
        const message = externalErrorKind(value) === 'unsupported' && error.httpStatus === 411 ? undefined : error.message
        for (const detail of [error.detail, message]) {
            if (typeof detail === 'string' && detail.trim() && !summary.includes(detail) && !details.includes(detail)) {
                details.push(detail)
            }
        }
    }
    return [summary, ...details].join('\n')
}

function externalErrorSummary(strings: ExternalStorageStrings, value: unknown): string {
    const kind = externalErrorKind(value)
    const oauthError = typeof value === 'object' && value !== null
        ? (value as { oauthError?: unknown }).oauthError
        : undefined
    const oauthErrorDescription = typeof value === 'object' && value !== null
        ? (value as { oauthErrorDescription?: unknown }).oauthErrorDescription
        : undefined
    const oauthSuffix = typeof oauthError === 'string'
        && oauthError.length > 0
        ? strings.oauthErrorCode.replace('{0}', oauthError)
        : ''
    const oauthDescriptionSuffix = typeof oauthErrorDescription === 'string'
        && oauthErrorDescription.length > 0
        ? strings.oauthErrorDescription.replace('{0}', oauthErrorDescription)
        : ''
    switch (kind) {
        case 'endpointRejected': return strings.endpointRejected
        case 'repositoryKeyUnavailable': return strings.unlockKey
        case 'deviceVaultUnavailable': return strings.deviceVaultUnavailable
        case 'clockSkew': return strings.clockSkew
        case 'folderTooLarge': return strings.folderTooLarge
        case 'repositoryBusy': return strings.repositoryBusy
        case 'locationOccupied': return strings.locationOccupied
        case 'authorizationTimedOut': return strings.authorizationTimedOut
        case 'localStorageFull': return strings.localStorageFull
        case 'localPermissionDenied': return strings.localPermissionDenied
        case 'localFailure': return strings.localFailure
        case 'authorizationUnavailable': return strings.authorizationUnavailable
        case 'folderNameConflict': return strings.folderNameConflict
        case 'folderCreateFailed': return strings.folderCreateFailed
        case 'folderInaccessible': return strings.folderInaccessible
        case 'folderNotRepository': return strings.folderNotRepository
        case 'folderUnsupportedLocation': return strings.folderUnsupportedLocation
        case 'alreadyConnected': return strings.connectionAlreadyAdded
        case 'unauthorized': return strings.credentialsRejected
        case 'reauthRequired': return [strings.reauthenticate, oauthSuffix, oauthDescriptionSuffix]
            .filter(Boolean)
            .join('\n')
        case 'notFound': return strings.repositoryNotFound
        case 'preconditionFailed': return strings.stateChanged
        case 'rateLimited':
        case 'dailyQuotaExhausted': return strings.requestBudget
        case 'storageFull': return strings.freeSpace
        case 'fileTooLarge': return strings.objectTooLarge
        case 'corrupt': return strings.corrupted
        case 'recoveryKeyMismatch': return strings.recoveryKeyMismatch
        case 'repositoryMismatch': return strings.repositoryMismatch
        case 'libraryFormatSwitchRequired': return strings.formatSwitchRequired
        case 'appUpdateRequired': return strings.appUpdateRequired
        case 'unsupported': return (value as { httpStatus?: unknown }).httpStatus === 411
            ? strings.requestLengthRequired : strings.unsupportedOperation
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

/** The folder a connection keeps its repository in, with the fixed app folders named as in the form. */
export function externalConnectionFolder(
    strings: ExternalStorageStrings,
    connection: Pick<ExternalConnectionSummary, 'providerId' | 'endpoint'>,
): string {
    const hint = connection.endpoint.repositoryHint
    if (connection.providerId === 'google_drive' && hint === 'appDataFolder') return strings.options['google_drive.space.appDataFolder']
    if (connection.providerId === 'onedrive' && hint === 'special/approot') return strings.options['onedrive.accountType.appFolder']
    return hint
}

/** Server and folder of a connection. Google Drive and OneDrive always use one API host, so it is left out. */
export function externalConnectionPlace(
    strings: ExternalStorageStrings,
    connection: Pick<ExternalConnectionSummary, 'providerId' | 'endpoint'>,
): string {
    const signedIn = connection.providerId === 'google_drive' || connection.providerId === 'onedrive'
    return [signedIn ? '' : connection.endpoint.authority, externalConnectionFolder(strings, connection)].filter(Boolean).join(' · ')
}
