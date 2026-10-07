#include <CoreFoundation/CoreFoundation.h>
#include <stdlib.h>
#include <string.h>

static CFDictionaryRef probe(CFURLRef url, CFArrayRef keys, CFErrorRef *error) {
    CFDictionaryRef original = CFURLCopyResourcePropertiesForKeys(url, keys, error);
    if (!original) return NULL;
    const char *mode = getenv("AXIAL_CAPACITY_PROBE");
    if (!mode) return original;
    CFRange range = CFRangeMake(0, CFArrayGetCount(keys));
    Boolean capacity_query = range.length == 2 &&
        CFArrayContainsValue(keys, range, kCFURLVolumeAvailableCapacityKey) &&
        CFArrayContainsValue(keys, range, kCFURLVolumeAvailableCapacityForImportantUsageKey);
    if (strcmp(mode, "wrong_type") == 0 && !capacity_query) return original;
    if (strcmp(mode, "failure") == 0 && capacity_query) {
        CFRelease(original);
        return NULL;
    }
    CFMutableDictionaryRef result = CFDictionaryCreateMutableCopy(NULL, 0, original);
    CFRelease(original);
    CFDictionaryRemoveValue(result, kCFURLVolumeAvailableCapacityKey);
    CFDictionaryRemoveValue(result, kCFURLVolumeAvailableCapacityForImportantUsageKey);
    if (strcmp(mode, "missing") == 0) return result;
    if (strcmp(mode, "wrong_type") == 0) {
        CFDictionarySetValue(result, kCFURLVolumeAvailableCapacityKey, kCFBooleanFalse);
        CFDictionarySetValue(result, kCFURLVolumeAvailableCapacityForImportantUsageKey, kCFBooleanFalse);
        return result;
    }
    int64_t important = strcmp(mode, "important") == 0 ? 4294967297LL :
        strcmp(mode, "negative") == 0 ? -1 : 0;
    int64_t available = strcmp(mode, "zero") == 0 ? 0 :
        strcmp(mode, "negative") == 0 ? -1 : 2147483648LL;
    CFNumberRef number = CFNumberCreate(NULL, kCFNumberSInt64Type, &important);
    if (strcmp(mode, "missing_important") != 0)
        CFDictionarySetValue(result, kCFURLVolumeAvailableCapacityForImportantUsageKey, number);
    CFRelease(number);
    number = CFNumberCreate(NULL, kCFNumberSInt64Type, &available);
    if (strcmp(mode, "important_zero_only") != 0)
        CFDictionarySetValue(result, kCFURLVolumeAvailableCapacityKey, number);
    CFRelease(number);
    return result;
}

__attribute__((used)) static struct { const void *replacement; const void *original; }
interpose __attribute__((section("__DATA,__interpose"))) = {
    (const void *)probe, (const void *)CFURLCopyResourcePropertiesForKeys
};
