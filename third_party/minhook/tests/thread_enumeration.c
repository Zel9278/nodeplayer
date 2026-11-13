// Exercise the production enumeration functions with live helper threads.
// Include hook.c so the test can check its private lists without exporting a
// production test API. Compile this file instead of hook.c for this standalone test.
#include <windows.h>
#include <processsnapshot.h>
#include <stdio.h>
static int fail_capture = 0;
static int fail_walk = 0;
static int walked = 0;
static int zero_count = 0;
static DWORD WINAPI probe_capture(HANDLE process, PSS_CAPTURE_FLAGS flags, DWORD context, HPSS *snapshot)
{
    if (fail_capture) return ERROR_NOT_SUPPORTED;
    return PssCaptureSnapshot(process, flags, context, snapshot);
}
static DWORD WINAPI probe_walk(HPSS snapshot, PSS_WALK_INFORMATION_CLASS type, HPSSWALK marker, void *entry, DWORD size)
{
    if (fail_walk && walked++ == 3) return ERROR_INVALID_DATA;
    return PssWalkSnapshot(snapshot, type, marker, entry, size);
}
static DWORD WINAPI probe_query(HPSS snapshot, PSS_QUERY_INFORMATION_CLASS type, void *information, DWORD size)
{
    DWORD status = PssQuerySnapshot(snapshot, type, information, size);
    if (status == ERROR_SUCCESS && zero_count && type == PSS_QUERY_THREAD_INFORMATION)
        ((PSS_THREAD_INFORMATION *)information)->ThreadsCaptured = 0;
    return status;
}
#define PssQuerySnapshot probe_query
#define PssCaptureSnapshot probe_capture
#define PssWalkSnapshot probe_walk
#include "../minhook/src/hook.c"
#undef PssCaptureSnapshot
#undef PssWalkSnapshot
static DWORD WINAPI wait_thread(void *event)
{
    WaitForSingleObject((HANDLE)event, INFINITE);
    return 0;
}
static int verify(PFROZEN_THREADS threads, DWORD *ids)
{
    UINT i, j;
    for (i = 0; i < threads->size; i++) {
        HANDLE thread;
        if (threads->pItems[i] == GetCurrentThreadId()) return 1;
        for (j = i + 1; j < threads->size; j++)
            if (threads->pItems[i] == threads->pItems[j]) return 2;
        thread = OpenThread(THREAD_QUERY_LIMITED_INFORMATION, FALSE, threads->pItems[i]);
        if (thread == NULL) return 3;
        if (GetProcessIdOfThread(thread) != GetCurrentProcessId()) return 4;
        CloseHandle(thread);
    }
    for (i = 0; i < 4; i++) {
        int found = 0;
        for (j = 0; j < threads->size; j++) found += threads->pItems[j] == ids[i];
        if (found != 1) return 5;
    }
    return 0;
}
int MH_TestProcessThreadEnumeration(void)
{
    HANDLE event, helpers[4];
    DWORD ids[4];
    int mode, result = 0;
    UINT i;
    if (MH_Initialize() != MH_OK) return 10;
    event = CreateEventW(NULL, TRUE, FALSE, NULL);
    if (event == NULL) return 11;
    for (i = 0; i < 4; i++) {
        helpers[i] = CreateThread(NULL, 0, wait_thread, event, 0, &ids[i]);
        if (helpers[i] == NULL) return 12;
    }
    for (mode = 0; mode < 4; mode++) {
        FROZEN_THREADS threads = {0};
        LARGE_INTEGER start, end, frequency;
        fail_capture = mode == 1;
        fail_walk = mode == 2;
        zero_count = mode == 3;
        walked = 0;
        QueryPerformanceFrequency(&frequency);
        QueryPerformanceCounter(&start);
        if (mode == 0 ? !EnumerateProcessThreads(&threads) : !EnumerateThreads(&threads)) result = 20 + mode;
        QueryPerformanceCounter(&end);
        if (result == 0) result = verify(&threads, ids);
        printf("mode=%d threads=%u elapsed_ms=%.3f result=%d\n", mode, threads.size,
            (double)(end.QuadPart - start.QuadPart) * 1000 / frequency.QuadPart, result);
        if (threads.pItems != NULL) HeapFree(g_hHeap, 0, threads.pItems);
        if (result != 0) break;
    }
    SetEvent(event);
    for (i = 0; i < 4; i++) { WaitForSingleObject(helpers[i], 1000); CloseHandle(helpers[i]); }
    CloseHandle(event);
    MH_Uninitialize();
    return result;
}

#ifdef MINHOOK_THREAD_ENUMERATION_MAIN
int main(void) { return MH_TestProcessThreadEnumeration(); }
#endif
