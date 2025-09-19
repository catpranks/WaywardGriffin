#include <dlfcn.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <cuda.h>
#include "NvFBC.h"

/// @brief Opaque handle to the internal capture state.
struct NvCaptureHandle
{
    void *lib_handle;
    NVFBC_API_FUNCTION_LIST fbc;
    NVFBC_SESSION_HANDLE fbc_handle;
};
typedef struct NvCaptureHandle NvCaptureHandle;

/// @brief Initializes the NvFBC library and prepares for frame capture.
/// @param handle_ptr A pointer to a handle that will be initialized.
/// @return NVFBC_SUCCESS on success, otherwise an error code.
NVFBCSTATUS nvcapture_init(NvCaptureHandle **handle_ptr)
{
    if (!handle_ptr)
    {
        return NVFBC_ERR_INVALID_PTR;
    }

    NVFBCSTATUS status = NVFBC_SUCCESS;
    NvCaptureHandle *handle = (NvCaptureHandle *)calloc(1, sizeof(NvCaptureHandle));
    if (!handle)
    {
        return NVFBC_ERR_OUT_OF_MEMORY;
    }

    handle->lib_handle = dlopen("libnvidia-fbc.so.1", RTLD_LAZY);
    if (!handle->lib_handle)
    {
        fprintf(stderr, "Failed to load libnvidia-fbc.so.1: %s\n", dlerror());
        status = NVFBC_ERR_INTERNAL;
        goto fail_handle;
    }

    PNVFBCCREATEINSTANCE create_instance = (PNVFBCCREATEINSTANCE)dlsym(handle->lib_handle, "NvFBCCreateInstance");
    if (!create_instance)
    {
        fprintf(stderr, "Failed to find NvFBCCreateInstance: %s\n", dlerror());
        status = NVFBC_ERR_INTERNAL;
        goto fail_lib;
    }

    memset(&handle->fbc, 0, sizeof(handle->fbc));
    handle->fbc.dwVersion = NVFBC_VERSION;

    status = create_instance(&handle->fbc);
    if (status != NVFBC_SUCCESS)
    {
        fprintf(stderr, "NvFBCCreateInstance failed with status: %d\n", status);
        goto fail_lib;
    }

    // This is the magic.
    uint32_t private_data[] = {0xAEF57AC5, 0x401D1A39, 0x1B856BBE, 0x9ED0CEBA};

    NVFBC_CREATE_HANDLE_PARAMS create_handle_params = {0};
    create_handle_params.dwVersion = NVFBC_CREATE_HANDLE_PARAMS_VER;
    create_handle_params.privateData = private_data;
    create_handle_params.privateDataSize = sizeof(private_data);
    create_handle_params.eBackend = NVFBC_BACKEND_X11;
    status = handle->fbc.nvFBCCreateHandle(&handle->fbc_handle, &create_handle_params);
    if (status != NVFBC_SUCCESS)
    {
        fprintf(stderr, "nvFBCCreateHandle failed: %s\n", handle->fbc.nvFBCGetLastErrorStr(handle->fbc_handle));
        goto fail_lib;
    }

    NVFBC_CREATE_CAPTURE_SESSION_PARAMS create_capture_params = {0};
    create_capture_params.dwVersion = NVFBC_CREATE_CAPTURE_SESSION_PARAMS_VER;
    create_capture_params.eCaptureType = NVFBC_CAPTURE_SHARED_CUDA;
    create_capture_params.eTrackingType = NVFBC_TRACKING_SCREEN;
    create_capture_params.bWithCursor = NVFBC_TRUE;
    create_capture_params.bPushModel = NVFBC_TRUE;
    create_capture_params.bAllowDirectCapture = NVFBC_TRUE;
    status = handle->fbc.nvFBCCreateCaptureSession(handle->fbc_handle, &create_capture_params);
    if (status != NVFBC_SUCCESS)
    {
        fprintf(stderr, "nvFBCCreateCaptureSession failed: %s\n", handle->fbc.nvFBCGetLastErrorStr(handle->fbc_handle));
        goto fail_fbc_handle;
    }

    NVFBC_TOCUDA_SETUP_PARAMS setup_params = {0};
    setup_params.dwVersion = NVFBC_TOCUDA_SETUP_PARAMS_VER;
    setup_params.eBufferFormat = NVFBC_BUFFER_FORMAT_BGRA;
    status = handle->fbc.nvFBCToCudaSetUp(handle->fbc_handle, &setup_params);
    if (status != NVFBC_SUCCESS)
    {
        fprintf(stderr, "nvFBCToCudaSetUp failed: %s\n", handle->fbc.nvFBCGetLastErrorStr(handle->fbc_handle));
        goto fail_capture_session;
    }

    *handle_ptr = handle;
    return NVFBC_SUCCESS;

fail_capture_session:
{
    NVFBC_DESTROY_CAPTURE_SESSION_PARAMS params = {.dwVersion = NVFBC_DESTROY_CAPTURE_SESSION_PARAMS_VER};
    handle->fbc.nvFBCDestroyCaptureSession(handle->fbc_handle, &params);
}
fail_fbc_handle:
{
    NVFBC_DESTROY_HANDLE_PARAMS params = {.dwVersion = NVFBC_DESTROY_HANDLE_PARAMS_VER};
    handle->fbc.nvFBCDestroyHandle(handle->fbc_handle, &params);
}
fail_lib:
    dlclose(handle->lib_handle);
fail_handle:
    free(handle);
    return status;
}

/// @brief Captures a single frame.
/// @param handle The handle returned by nvcapture_init.
/// @param dptr A pointer to a CUDA device pointer that will receive the frame data.
/// @param info A pointer to a struct that will be filled with frame metadata.
/// @return NVFBC_SUCCESS on success, otherwise an error code.
NVFBCSTATUS nvcapture_capture(NvCaptureHandle *handle, CUdeviceptr *dptr, NVFBC_FRAME_GRAB_INFO *info, uint32_t timeout_ms)
{
    if (!handle)
    {
        return NVFBC_ERR_INVALID_PTR;
    }
    NVFBC_TOCUDA_GRAB_FRAME_PARAMS params = {0};
    params.dwVersion = NVFBC_TOCUDA_GRAB_FRAME_PARAMS_VER;
    if (timeout_ms == 0)
    {
        params.dwFlags = NVFBC_TOCUDA_GRAB_FLAGS_NOWAIT;
    }
    else
    {
        params.dwFlags = NVFBC_TOCUDA_GRAB_FLAGS_NOWAIT_IF_NEW_FRAME_READY;
    }
    params.pCUDADeviceBuffer = dptr;
    params.pFrameGrabInfo = info;
    params.dwTimeoutMs = timeout_ms;
    return handle->fbc.nvFBCToCudaGrabFrame(handle->fbc_handle, &params);
}

/// @brief Binds the NvFBC context to the current thread.
/// @param handle The handle returned by nvcapture_init.
/// @return NVFBC_SUCCESS on success, otherwise an error code.
NVFBCSTATUS nvcapture_bind_thread(NvCaptureHandle *handle)
{
    if (!handle)
    {
        return NVFBC_ERR_INVALID_PTR;
    }
    NVFBC_BIND_CONTEXT_PARAMS params = {0};
    params.dwVersion = NVFBC_BIND_CONTEXT_PARAMS_VER;
    return handle->fbc.nvFBCBindContext(handle->fbc_handle, &params);
}

/// @brief Releases the NvFBC context from the current thread.
/// @param handle The handle returned by nvcapture_init.
/// @return NVFBC_SUCCESS on success, otherwise an error code.
NVFBCSTATUS nvcapture_release_thread(NvCaptureHandle *handle)
{
    if (!handle)
    {
        return NVFBC_ERR_INVALID_PTR;
    }
    NVFBC_RELEASE_CONTEXT_PARAMS params = {0};
    params.dwVersion = NVFBC_RELEASE_CONTEXT_PARAMS_VER;
    return handle->fbc.nvFBCReleaseContext(handle->fbc_handle, &params);
}

/// @brief Releases all resources associated with the capture handle.
/// @param handle The handle to be destroyed.
/// @return NVFBC_SUCCESS on success, otherwise an error code.
NVFBCSTATUS nvcapture_destroy(NvCaptureHandle *handle)
{
    if (!handle)
    {
        return NVFBC_ERR_INVALID_PTR;
    }

    if (handle->fbc_handle)
    {
        NVFBC_DESTROY_CAPTURE_SESSION_PARAMS destroy_capture_params = {0};
        destroy_capture_params.dwVersion = NVFBC_DESTROY_CAPTURE_SESSION_PARAMS_VER;
        handle->fbc.nvFBCDestroyCaptureSession(handle->fbc_handle, &destroy_capture_params);

        NVFBC_DESTROY_HANDLE_PARAMS destroy_handle_params = {0};
        destroy_handle_params.dwVersion = NVFBC_DESTROY_HANDLE_PARAMS_VER;
        handle->fbc.nvFBCDestroyHandle(handle->fbc_handle, &destroy_handle_params);
    }

    if (handle->lib_handle)
    {
        dlclose(handle->lib_handle);
    }
    free(handle);
    return NVFBC_SUCCESS;
}