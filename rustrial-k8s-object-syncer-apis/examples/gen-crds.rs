use kube::CustomResourceExt;
use rustrial_k8s_object_syncer_apis::ObjectSync;
use serde_yaml_ng;

pub fn main() {
    println!("{}", serde_yaml_ng::to_string(&ObjectSync::crd()).unwrap());
}
