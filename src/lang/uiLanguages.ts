export const UI_LANGUAGES = [
    { value: 'en', label: 'English' },
    { value: 'ko', label: '한국어' },
]

/** The offered UI language for a device language tag such as `ko-KR`, or English. */
export function uiLanguageFor(locale: string): string {
    const primary = locale.split('-')[0]
    return UI_LANGUAGES.find((option) => option.value === primary)?.value ?? 'en'
}
