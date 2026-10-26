//! `reef.api.v1.FileService`: declared in R2.01, implemented by R2.04. Until
//! then every RPC answers `UNIMPLEMENTED` with reason `NOT_IMPLEMENTED`.

use reef_api::v1::file_service_server::FileService;
use reef_api::v1::{
    EditFilesRequest, EditFilesResponse, ListFilesRequest, ListFilesResponse, ReadFileRequest,
    ReadFileResponse,
};
use tonic::{Request, Response, Status};

use crate::error::not_implemented;

/// Serves `FileService`.
#[derive(Debug, Default)]
pub struct FilesHandler;

#[tonic::async_trait]
impl FileService for FilesHandler {
    async fn read_file(
        &self,
        _request: Request<ReadFileRequest>,
    ) -> Result<Response<ReadFileResponse>, Status> {
        Err(not_implemented("/reef.api.v1.FileService/ReadFile"))
    }

    async fn list_files(
        &self,
        _request: Request<ListFilesRequest>,
    ) -> Result<Response<ListFilesResponse>, Status> {
        Err(not_implemented("/reef.api.v1.FileService/ListFiles"))
    }

    async fn edit_files(
        &self,
        _request: Request<EditFilesRequest>,
    ) -> Result<Response<EditFilesResponse>, Status> {
        Err(not_implemented("/reef.api.v1.FileService/EditFiles"))
    }
}
