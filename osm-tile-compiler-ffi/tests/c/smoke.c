/*
 * A C host for the C ABI: compiles the PBF given as argv[1] into argv[2]
 * (a directory), polling until the job ends. Built and run by
 * scripts/check-c-abi.sh against the static library.
 */
#include "osm_tile_compiler.h"

#include <stdio.h>
#include <string.h>

int main(int argc, char **argv) {
    if (argc != 3) {
        fprintf(stderr, "usage: smoke <input.osm.pbf> <output-dir>\n");
        return 2;
    }
    if (otc_abi_version() != OTC_ABI_VERSION) {
        fprintf(stderr, "ABI %u, header %d\n", otc_abi_version(), OTC_ABI_VERSION);
        return 1;
    }
    printf("osm-tile-compiler %s\n", otc_version());

    char *error = NULL;
    OtcJob *job = NULL;
    if (otc_job_start("{\"abiVersion\":1,\"kind\":\"nope\"}", &job, &error) !=
            OTC_ERROR_INVALID_REQUEST || job != NULL || error == NULL) {
        fprintf(stderr, "a bad request started\n");
        return 1;
    }
    printf("rejected: %s\n", error);
    otc_string_free(error);
    if (otc_validate("{\"abiVersion\":1,\"kind\":\"nope\"}", &error) !=
            OTC_ERROR_INVALID_REQUEST || error == NULL) {
        fprintf(stderr, "a bad request validated\n");
        return 1;
    }
    otc_string_free(error);

    char request[4096];
    snprintf(request, sizeof request,
             "{\"abiVersion\":1,\"kind\":\"compile_basemap\",\"request\":{"
             "\"inputPbf\":\"%s\",\"output\":\"%s/smoke.pmtiles\","
             "\"sources\":\"%s/sources\",\"tempDir\":\"%s/temp\","
             "\"omitSources\":[\"water_polygons\",\"natural_earth\",\"lake_centerlines\"],"
             "\"threads\":2,\"zoom\":{\"max\":10}}}",
             argv[1], argv[2], argv[2], argv[2]);
    if (otc_job_start(request, &job, &error) != OTC_OK) {
        fprintf(stderr, "start failed: %s\n", error);
        otc_string_free(error);
        return 1;
    }
    int32_t state;
    while ((state = otc_job_wait(job, 200)) == OTC_JOB_RUNNING) {
        char *poll = otc_job_poll(job);
        printf("poll: %s\n", poll);
        otc_string_free(poll);
    }
    char *result = NULL;
    if (otc_job_result(job, &result) != OTC_OK) {
        fprintf(stderr, "no result\n");
        return 1;
    }
    int ok = state == OTC_JOB_SUCCEEDED && strstr(result, "\"ok\":true") != NULL;
    printf("result (%d): %.300s\n", state, result);
    otc_string_free(result);
    otc_job_free(job);
    return ok ? 0 : 1;
}
